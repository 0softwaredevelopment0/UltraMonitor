# UltraMonitor

![Rust](https://img.shields.io/badge/Rust-1.85%2B-orange) ![Build](https://img.shields.io/badge/build-Cargo-green) ![Development status](https://img.shields.io/badge/status-Beta-yellow) ![License](https://img.shields.io/badge/License-AGPLv3-blue)

**A portable desktop utility for monitoring your hardware sensors and stress testing your system.**

---

### Organization Docs

[![Guide](https://img.shields.io/badge/Guide-0softwaredevelopment0-00AEFF)](https://github.com/0softwaredevelopment0/.github/blob/main/GUIDE.md) · [![Contributing](https://img.shields.io/badge/Contributing-0softwaredevelopment0-4CAF50)](https://github.com/0softwaredevelopment0/.github/blob/main/CONTRIBUTING.md) · [![Security](https://img.shields.io/badge/Security-0softwaredevelopment0-D9534F)](https://github.com/0softwaredevelopment0/.github/blob/main/SECURITY.md) · [![Code of Conduct](https://img.shields.io/badge/Code%20of%20Conduct-0softwaredevelopment0-5BC0DE)](https://github.com/0softwaredevelopment0/.github/blob/main/CODE_OF_CONDUCT.md)

UltraMonitor shows real-time readings from your CPU, memory, disks and network, and can put
your system under load with CPU / RAM / disk / GPU stress tests to check stability and cooling.
Built with **Rust**, **`egui`/`eframe`** (wgpu) and **`sysinfo`** — a single self-contained binary,
no installation required.

---

## Features

- **Toggle switches** open the *Sensors* and *Stress Test* views, which can stay open side by
  side so you can watch the sensors while stressing.
- **Sensors view, two tabs:**
  - *Sensors* — a live `Current | Min | Avg | Max` table: CPU temperature, load and frequency,
    per-core load and frequency, RAM and swap usage, disk usage plus read/write rates, network
    download/upload rates, and component temperatures.
  - *System Info* — the hardware inventory grouped by section (OS, CPU, memory, disks, network,
    thermal sensors), with live values highlighted.
- **Configurable refresh rate** — from **1 to 10,000 ms**; persisted to `config.json` next to
  the executable.
- **Stress test** — four tests with their own parameters:
  - *CPU* — full load across the selected threads (FMA / transcendental / integer / cache-buster
    workloads);
  - *RAM* — allocate and constantly read/write memory buffers (slider: % of available RAM);
  - *Disk* — parallel sequential writes/reads plus random 64 KB I/O on a temporary file
    (slider: file size in MB);
  - *GPU* — a compute-heavy Mandelbrot fractal render scaled by a five-level selector
    (Light → Meltdown), producing a sustained floating-point load.
  - Optional duration in seconds (empty = run until stopped manually; max 86,400 s),
    **Start / Stop** buttons, live CPU load, RAM load and temperature gauges, a progress bar and
    **automatic stop at 90 °C**.
- **Dark UI**, a live "LIVE" indicator, and the custom **`app-avatar.png`** icon.
- **Portable** — a single self-contained binary.
- **Headless self-test** for CI / smoke tests: `ultramonitor --selftest`.
- **Stress-test CSV report** — after a run, click **Report** to export the recorded samples
  (elapsed, CPU load, RAM load, CPU temperature) as a CSV.
- **Headless stress CLI** — run the stress engine from a terminal (see below).

> Note: on Windows, CPU temperature and fan readings require a driver such as
> [LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor).
> Without one, those sensors show «—». `sysinfo` does not expose battery / fan / voltage, so
> those sensors are simply omitted.

---

## Requirements

- **Rust 1.85+** (stable). On Windows the **MSVC** toolchain
  (`stable-x86_64-pc-windows-msvc`) with Visual Studio Build Tools is recommended; the GNU
  toolchain also works when MinGW-w64 `gcc` is installed.

## Build

```bash
cargo build --release
```

The self-contained binary lands at `target/release/ultramonitor(.exe)`.

## Run

```bash
cargo run --release
```

Or from a terminal:

```bash
target/release/ultramonitor
```

## Self-test (CI / smoke tests)

```bash
ultramonitor --selftest
```

A headless mode: probes the sensors, prints the readings and system info, and exits with
code `0` on success.

## Stress test CLI

Run the same stress engine from a terminal, no GUI:

```bash
ultramonitor stress --cpu --ram --duration 60 --report stress.csv
```

| Flag | Meaning |
|------|---------|
| `--cpu` / `--ram` / `--disk` / `--gpu` | which tests to run (at least one required) |
| `--duration SEC` | stop after `SEC` seconds (default: until temperature limit) |
| `--report FILE.csv` | write a CSV report (elapsed, CPU %, RAM %, CPU temp) |
| `--temp-limit C` | auto-stop temperature in °C (default: 90) |

## Configuration

`config.json` is stored next to the executable (or under `~/.ultramonitor` for `cargo run`
dev builds):

```json
{ "refreshIntervalMs": 1000 }
```

- `refreshIntervalMs` — sensor refresh interval in milliseconds (1–10,000).

---

## Project layout

```
src/
├── main.rs            entry point (GUI + --selftest + stress CLI)
├── ui.rs              egui GUI: top bar, sensors + stress views, dialogs
├── config.rs          portable config.json persistence
├── format.rs          unit & time formatting helpers
├── monitoring.rs      sysinfo-backed sensors + system inventory
├── stress.rs          CPU / RAM / disk / GPU stress engine + runner + reporter
└── lib.rs             library root
resources/
└── app-avatar.png     bundled 512×512 app icon
```

---

## Icon

The application icon is the bundled **`resources/app-avatar.png`** (512×512), baked into the
binary at compile time and shown on the window and taskbar.

---

## License

Licensed under the **GNU Affero General Public License v3.0 (AGPL-3.0)** — see [LICENSE](LICENSE).
