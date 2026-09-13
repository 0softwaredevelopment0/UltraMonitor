//! UltraMonitor binary: dispatches to the GUI, the `--selftest` hardware probe
//! or the headless `stress` CLI.

mod ui;

use std::time::Duration;

use ultramonitor::monitoring::Monitor;
use ultramonitor::stress::{
    CpuStress, DiskStress, GpuLevel, GpuStress, MemoryStress, StressReporter, StressRunner,
    StressTest,
};

const DEFAULT_TEMP_LIMIT_C: f64 = 90.0;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        match args[1].as_str() {
            "--selftest" => std::process::exit(if run_selftest() { 0 } else { 1 }),
            "stress" | "--stress" => {
                std::process::exit(if run_stress_cli(&args) { 0 } else { 1 })
            }
            // Any other flag falls through to the GUI (same as the Java launcher).
            _ => {}
        }
    }
    if let Err(e) = ui::run_gui() {
        eprintln!("Failed to start GUI: {e}");
        std::process::exit(1);
    }
}

// ------------------------------------------------------------ CLI probe --

fn run_selftest() -> bool {
    println!("UltraMonitor self-test");
    println!("Rust: rustc {}", env!("CARGO_PKG_VERSION"));
    let mut monitor = Monitor::new();
    monitor.sample(); // prime rate counters
    std::thread::sleep(Duration::from_millis(750));
    let readings = monitor.sample();
    println!("Sensors: {}", readings.len());
    for r in &readings {
        let value = if r.available() {
            format!("{:.1} {}", r.value, r.unit)
        } else {
            "n/a".to_string()
        };
        println!("  {:<28} {}", r.name, value);
    }
    let info = monitor.system_info();
    println!("System info entries: {}", info.len());
    for e in info.iter().take(6) {
        println!("  [{}] {:<22} {}", e.section, e.label, e.value);
    }
    true
}

// ------------------------------------------------------------ CLI stress --

fn run_stress_cli(raw_args: &[String]) -> bool {
    let mut cpu = false;
    let mut ram = false;
    let mut disk = false;
    let mut gpu = false;
    let mut duration: u64 = 0;
    let mut report: Option<std::path::PathBuf> = None;
    let mut temp_limit = DEFAULT_TEMP_LIMIT_C;

    let mut i = 2;
    while i < raw_args.len() {
        match raw_args[i].as_str() {
            "--cpu" => cpu = true,
            "--ram" | "--memory" => ram = true,
            "--disk" => disk = true,
            "--gpu" => gpu = true,
            "--duration" => {
                if i + 1 < raw_args.len() {
                    duration = raw_args[i + 1].parse().unwrap_or(0);
                    i += 1;
                }
            }
            "--report" => {
                if i + 1 < raw_args.len() {
                    report = Some(std::path::PathBuf::from(&raw_args[i + 1]));
                    i += 1;
                }
            }
            "--temp-limit" => {
                if i + 1 < raw_args.len() {
                    temp_limit = raw_args[i + 1].parse().unwrap_or(DEFAULT_TEMP_LIMIT_C);
                    i += 1;
                }
            }
            "--help" | "-h" => {
                print_stress_help();
                return true;
            }
            other => {
                eprintln!("Unknown stress flag: {other}");
                print_stress_help();
                return false;
            }
        }
        i += 1;
    }

    if !cpu && !ram && !disk && !gpu {
        eprintln!("Select at least one test: --cpu, --ram, --disk, --gpu");
        print_stress_help();
        return false;
    }

    let mut tests: Vec<Box<dyn StressTest>> = Vec::new();
    if cpu {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        tests.push(Box::new(CpuStress::new(cores)));
    }
    if ram {
        tests.push(Box::new(MemoryStress::new(50)));
    }
    if disk {
        tests.push(Box::new(DiskStress::new(512)));
    }
    if gpu {
        tests.push(Box::new(GpuStress::new(GpuLevel::Intense)));
    }

    println!("=== UltraMonitor stress run ===");
    println!(
        "Tests: {}",
        tests.iter().map(|t| t.name()).collect::<Vec<_>>().join(" + ")
    );
    println!(
        "Duration: {}",
        if duration > 0 {
            format!("{duration} s")
        } else {
            "until temperature limit".to_string()
        }
    );
    println!("Temp limit: {temp_limit:.0} °C");

    let reporter = StressReporter::new();
    reporter.start();
    let runner = StressRunner::new(tests);
    runner.start(duration).map_err(|e| eprintln!("{e}")).ok();

    let mut monitor = Monitor::new();
    let deadline = if duration > 0 {
        Some(std::time::Instant::now() + Duration::from_secs(duration))
    } else {
        None
    };
    let mut reason: Option<String> = None;

    while runner.is_running() {
        let (cpu_load, ram_load, temp) = {
            let readings = monitor.sample();
            (
                value(&readings, "cpu.load"),
                value(&readings, "ram.load"),
                value(&readings, "cpu.temp"),
            )
        };
        reporter.tick(cpu_load, ram_load, temp);

        if temp >= 0.0 && temp >= temp_limit {
            reason = Some(format!(
                "CPU temperature {temp:.1} °C >= limit {temp_limit:.0} °C — stopped automatically"
            ));
            runner.stop(&reason.clone().unwrap());
            break;
        }
        if let Some(dl) = deadline {
            if std::time::Instant::now() >= dl {
                reason = Some(format!("Duration reached ({duration} s)"));
                runner.stop(&reason.clone().unwrap());
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    if reason.is_none() {
        reason = Some(if runner.stop_reason().is_empty() {
            "finished".to_string()
        } else {
            runner.stop_reason()
        });
    }

    println!();
    println!("--- Summary ---");
    println!(
        "Ran for ~{} s  |  {}",
        runner.elapsed_seconds(),
        reason.clone().unwrap_or_default()
    );
    for line in reporter.summary(runner.elapsed_seconds()) {
        println!("{line}");
    }

    if let Some(path) = report {
        let ok = reporter.write_csv(&path, reason.as_deref());
        println!(
            "{}",
            if ok {
                format!("Report written: {}", path.display())
            } else {
                format!("Failed to write report: {}", path.display())
            }
        );
    }
    true
}

fn value(readings: &[ultramonitor::monitoring::SensorReading], key: &str) -> f64 {
    readings
        .iter()
        .find(|r| r.key == key)
        .map(|r| r.value)
        .unwrap_or(f64::NAN)
}

fn print_stress_help() {
    println!();
    println!("UltraMonitor stress — headless hardware stress run");
    println!();
    println!("Usage:");
    println!("  ultramonitor stress [options]");
    println!();
    println!("Options:");
    println!("  --cpu               run CPU load test");
    println!("  --ram               run RAM load test");
    println!("  --disk              run disk load test");
    println!("  --gpu               run GPU load test");
    println!("  --duration SEC      stop after SEC seconds (default: until temp limit)");
    println!("  --report FILE.csv   write a CSV report (elapsed,cpu%,ram%,tempC)");
    println!("  --temp-limit C      auto-stop temperature in °C (default: 90)");
    println!("  --help, -h          show this help");
    println!();
    println!("Example:");
    println!("  ultramonitor stress --cpu --ram --duration 60 --report stress.csv");
    println!();
}
