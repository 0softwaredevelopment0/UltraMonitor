//! The egui GUI: a top bar with two toggle switches (Sensors / Stress Test)
//! and the two live views, sharing one `Monitor` so sampling runs on a single
//! cadence (fast while a stress test runs, the configured interval otherwise).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Layout, RichText};
use ultramonitor::config;
use ultramonitor::format;
use ultramonitor::monitoring::{InfoEntry, LiveStats, Monitor, SensorRow};
use ultramonitor::stress::{
    CpuStress, DiskStress, GpuLevel, GpuStress, MemoryStress, StressReporter, StressRunner,
    StressTest,
};

// ---- dark palette (same family as the rest of the rizer001 apps) ----
const BG: Color32 = Color32::from_rgb(17, 18, 26);
const PANEL: Color32 = Color32::from_rgb(27, 28, 40);
const PANEL2: Color32 = Color32::from_rgb(32, 34, 48);
const FIELD: Color32 = Color32::from_rgb(40, 42, 58);
const ACCENT: Color32 = Color32::from_rgb(124, 108, 255);
const TEXT: Color32 = Color32::from_rgb(226, 227, 240);
const MUTED: Color32 = Color32::from_rgb(140, 143, 165);
const GREEN: Color32 = Color32::from_rgb(88, 230, 140);
const RED: Color32 = Color32::from_rgb(255, 105, 105);
const BLUE: Color32 = Color32::from_rgb(94, 160, 255);

const AUTO_STOP_TEMP_C: f64 = 90.0;
const HIDE_UNAVAILABLE_GRACE: Duration = Duration::from_secs(5);
const STRESS_TICK: Duration = Duration::from_millis(500);
const SYSTEM_INFO_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
enum SensorTab {
    Sensors,
    SystemInfo,
}

struct StressState {
    cpu_on: bool,
    ram_on: bool,
    disk_on: bool,
    gpu_on: bool,
    cpu_threads: usize,
    ram_percent: usize,
    disk_mb: u64,
    gpu_level: GpuLevel,
    duration_field: String,
    runner: Option<StressRunner>,
    reporter: StressReporter,
    cpu_load: f64,
    ram_load: f64,
    temp: f64,
    status: String,
    confirm_open: bool,
    last_running: bool,
}

impl Default for StressState {
    fn default() -> Self {
        StressState {
            cpu_on: false,
            ram_on: false,
            disk_on: false,
            gpu_on: false,
            cpu_threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
            ram_percent: 50,
            disk_mb: 512,
            gpu_level: GpuLevel::Medium,
            duration_field: String::new(),
            runner: None,
            reporter: StressReporter::new(),
            cpu_load: f64::NAN,
            ram_load: f64::NAN,
            temp: f64::NAN,
            status: "Select one or more tests and press Start.".to_string(),
            confirm_open: false,
            last_running: false,
        }
    }
}

impl StressState {
    fn running(&self) -> bool {
        self.runner.as_ref().map(|r| r.is_running()).unwrap_or(false)
    }

    fn build_tests(&self) -> Vec<Box<dyn StressTest>> {
        let mut tests: Vec<Box<dyn StressTest>> = Vec::new();
        if self.cpu_on {
            tests.push(Box::new(CpuStress::new(self.cpu_threads.max(1))));
        }
        if self.ram_on {
            tests.push(Box::new(MemoryStress::new(self.ram_percent)));
        }
        if self.disk_on {
            tests.push(Box::new(DiskStress::new(self.disk_mb.max(64))));
        }
        if self.gpu_on {
            tests.push(Box::new(GpuStress::new(self.gpu_level)));
        }
        tests
    }

    fn parse_duration(&self) -> Option<u64> {
        let text = self.duration_field.trim();
        if text.is_empty() {
            return Some(0);
        }
        match text.parse::<u64>() {
            Ok(v) if v <= 86_400 => Some(v),
            _ => None,
        }
    }

    fn any_selected(&self) -> bool {
        self.cpu_on || self.ram_on || self.disk_on || self.gpu_on
    }
}

pub struct App {
    show_sensors: bool,
    show_stress: bool,

    monitor: Monitor,
    stats: LiveStats,
    rows: Vec<SensorRow>,
    info_entries: Vec<InfoEntry>,
    ever_available: HashSet<String>,
    opened_at: Instant,
    last_sample: Instant,
    last_system_info: Instant,
    sensor_tab: SensorTab,
    interval_ms: i64,
    interval_field: String,
    saved_flash: Option<Instant>,
    interval_error: bool,

    stress: StressState,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let interval_ms = config::load_interval_ms();
        let mut app = App {
            show_sensors: false,
            show_stress: false,
            monitor: Monitor::new(),
            stats: LiveStats::default(),
            rows: Vec::new(),
            info_entries: Vec::new(),
            ever_available: HashSet::new(),
            opened_at: Instant::now(),
            last_sample: Instant::now(),
            last_system_info: Instant::now(),
            sensor_tab: SensorTab::Sensors,
            interval_ms,
            interval_field: interval_ms.to_string(),
            saved_flash: None,
            interval_error: false,
            stress: StressState::default(),
        };
        // Prime the monitor so the first sample has real deltas.
        app.sample();
        app.last_sample = Instant::now();
        app
    }

    // ---- sampling (single cadence) ----

    fn sample(&mut self) {
        let readings = self.monitor.sample();
        let grace = self.opened_at.elapsed() < HIDE_UNAVAILABLE_GRACE;
        let mut rows = Vec::with_capacity(readings.len());
        for r in &readings {
            self.stats.update(&r.key, r.value);
            if r.available() {
                self.ever_available.insert(r.key.clone());
            }
            let show = r.available() || grace || self.ever_available.contains(&r.key);
            if show {
                rows.push(SensorRow::of(r, &self.stats));
            }
        }
        self.rows = rows;

        // Feed the stress gauges from the same sample.
        self.stress.cpu_load = value(&readings, "cpu.load");
        self.stress.ram_load = value(&readings, "ram.load");
        self.stress.temp = value(&readings, "cpu.temp");
    }

    fn stress_tick(&mut self) {
        if !self.stress.running() {
            // Detect a just-finished run (user stop, temp stop or duration).
            if self.stress.last_running {
                self.stress.last_running = false;
                let reason = self
                    .stress
                    .runner
                    .as_ref()
                    .map(|r| r.stop_reason())
                    .unwrap_or_else(|| "Stopped".to_string());
                self.stress.status = format!("Finished — {reason}");
            }
            return;
        }
        self.stress.last_running = true;
        let cpu = self.stress.cpu_load;
        let ram = self.stress.ram_load;
        let temp = self.stress.temp;
        self.stress.reporter.tick(cpu, ram, temp);

        let runner = self.stress.runner.as_ref().unwrap();
        let summary = runner
            .tests()
            .iter()
            .map(|t| format!("{}: {}", t.name(), t.status()))
            .collect::<Vec<_>>()
            .join("  ·  ");
        self.stress.status = summary;

        if temp > 0.0 && temp >= AUTO_STOP_TEMP_C {
            runner.stop("CPU temperature reached 90 °C — stopped automatically");
        } else if runner.is_finished_by_time() {
            runner.stop("Duration reached");
        }
    }

    fn sample_system_info(&mut self) {
        self.info_entries = self.monitor.system_info();
    }

    // ---- UI ----

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Frame::NONE
            .fill(PANEL)
            .corner_radius(12)
            .inner_margin(egui::Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // Brand.
                    egui::Frame::NONE
                        .fill(ACCENT)
                        .corner_radius(8)
                        .inner_margin(egui::Margin::same(8))
                        .show(ui, |ui| {
                            ui.label(RichText::new("UM").color(Color32::WHITE).strong());
                        });
                    ui.vertical(|ui| {
                        ui.label(RichText::new("UltraMonitor").color(TEXT).size(18.0).strong());
                        ui.label(
                            RichText::new("Real-time hardware monitoring & stress testing")
                                .color(MUTED)
                                .size(11.0),
                        );
                    });

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        // LIVE pill.
                        egui::Frame::NONE
                            .fill(PANEL2)
                            .corner_radius(10)
                            .inner_margin(egui::Margin::symmetric(10, 5))
                            .show(ui, |ui| {
                                let pulse = (self.opened_at.elapsed().as_millis() / 450) % 2 == 0;
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new("●")
                                            .color(if pulse { GREEN } else { GREEN.gamma_multiply(0.35) })
                                            .size(9.0),
                                    );
                                    ui.label(RichText::new("LIVE").color(GREEN).strong().size(11.0));
                                });
                            });

                        // Stress toggle.
                        toggle_row(ui, "Stress Test", &mut self.show_stress);

                        // Sensors toggle.
                        toggle_row(ui, "Sensors", &mut self.show_sensors);
                    });
                });
            });
    }

    fn welcome(&self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(60.0);
            ui.label(RichText::new("Live sensors for your CPU, memory, disks and network").color(TEXT).size(16.0));
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Plus CPU, RAM, disk and GPU stress tests to push your hardware to its limits.\n\
                     Portable and open source. Flip a switch above to begin.",
                )
                .color(MUTED),
            );
        });
    }

    fn sensors_view(&mut self, ui: &mut egui::Ui) {
        egui::Frame::NONE
            .fill(PANEL)
            .corner_radius(12)
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.sensor_tab, SensorTab::Sensors, "Sensors");
                    ui.selectable_value(&mut self.sensor_tab, SensorTab::SystemInfo, "System Info");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("Updating every {} ms", self.interval_ms))
                                .color(MUTED),
                        );
                    });
                });
                ui.add_space(6.0);

                match self.sensor_tab {
                    SensorTab::Sensors => self.sensor_table(ui),
                    SensorTab::SystemInfo => self.system_info_table(ui),
                }

                ui.separator();
                self.interval_bar(ui);
            });
    }

    fn sensor_table(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(360.0)
            .show(ui, |ui| {
                egui::Grid::new("sensor_grid")
                    .num_columns(5)
                    .striped(true)
                    .min_col_width(40.0)
                    .spacing([18.0, 4.0])
                    .show(ui, |ui| {
                        for h in ["Sensor", "Current", "Min", "Avg", "Max"] {
                            ui.label(RichText::new(h).color(MUTED).strong());
                        }
                        ui.end_row();
                        for row in &self.rows {
                            ui.label(RichText::new(&row.name).color(TEXT));
                            let c = if row.available { TEXT } else { MUTED };
                            ui.label(RichText::new(&row.current).color(c).monospace());
                            ui.label(RichText::new(&row.min).color(MUTED).monospace());
                            ui.label(RichText::new(&row.avg).color(MUTED).monospace());
                            ui.label(RichText::new(&row.max).color(MUTED).monospace());
                            ui.end_row();
                        }
                    });
            });
    }

    fn system_info_table(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(360.0)
            .show(ui, |ui| {
                let mut current = String::new();
                for entry in &self.info_entries {
                    if entry.section != current {
                        current = entry.section.clone();
                        ui.add_space(6.0);
                        ui.label(RichText::new(&entry.section).color(ACCENT).strong());
                    }
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("  {}", entry.label))
                                .color(TEXT)
                                .size(12.0),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let value_color = if entry.live { BLUE } else { MUTED };
                            ui.label(
                                RichText::new(&entry.value)
                                    .color(value_color)
                                    .monospace()
                                    .size(12.0),
                            );
                        });
                    });
                }
            });
    }

    fn interval_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Update rate (ms):").color(MUTED));
            let field = egui::TextEdit::singleline(&mut self.interval_field)
                .desired_width(70.0);
            let resp = if self.interval_error {
                ui.add(field.text_color(RED))
            } else {
                ui.add(field)
            };
            if resp.changed() {
                self.interval_error = false;
            }
            ui.label(RichText::new("(1 – 10,000)").color(MUTED).size(11.0));

            if ui.button("Save").clicked() {
                self.save_interval();
            }
            if let Some(flash) = self.saved_flash {
                if flash.elapsed() < Duration::from_millis(1800) {
                    ui.label(RichText::new("✓ Saved").color(GREEN));
                } else {
                    self.saved_flash = None;
                }
            }
            if self.interval_error {
                ui.label(RichText::new("Value must be 1 – 10,000").color(RED));
            }
        });
    }

    fn save_interval(&mut self) {
        let value: i64 = self.interval_field.trim().parse().unwrap_or(-1);
        if !(config::MIN_INTERVAL_MS..=config::MAX_INTERVAL_MS).contains(&value) {
            self.interval_error = true;
            return;
        }
        self.interval_error = false;
        if config::save_interval_ms(value) {
            self.interval_ms = value;
            self.saved_flash = Some(Instant::now());
        } else {
            self.interval_error = true;
        }
    }

    // ---- stress view ----

    fn stress_view(&mut self, ui: &mut egui::Ui) {
        egui::Frame::NONE
            .fill(PANEL)
            .corner_radius(12)
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.label(RichText::new("Stress Test").color(TEXT).strong().size(14.0));
                ui.add_space(6.0);

                let running = self.stress.running();
                ui.add_enabled_ui(!running, |ui| {
                    Self::test_card(
                        ui,
                        "CPU",
                        "Full floating-point load on every selected core",
                        &mut self.stress.cpu_on,
                    );
                    if self.stress.cpu_on {
                        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Threads").color(MUTED).size(11.0));
                            ui.add(egui::Slider::new(&mut self.stress.cpu_threads, 1..=cores.max(1)));
                            ui.label(
                                RichText::new(cores_label(self.stress.cpu_threads, cores))
                                    .color(MUTED)
                                    .size(11.0),
                            );
                        });
                    }

                    Self::test_card(ui, "RAM", "Allocate and constantly read/write memory buffers", &mut self.stress.ram_on);
                    if self.stress.ram_on {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Size").color(MUTED).size(11.0));
                            ui.add(egui::Slider::new(&mut self.stress.ram_percent, 10..=80));
                            ui.label(
                                RichText::new(format!("{}% of available RAM", self.stress.ram_percent))
                                    .color(MUTED)
                                    .size(11.0),
                            );
                        });
                    }

                    Self::test_card(ui, "Disk", "Sequential writes and reads on a temporary file", &mut self.stress.disk_on);
                    if self.stress.disk_on {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("File size").color(MUTED).size(11.0));
                            ui.add(egui::Slider::new(&mut self.stress.disk_mb, 256..=4096));
                            ui.label(
                                RichText::new(format!("{} MB temp file", self.stress.disk_mb))
                                    .color(MUTED)
                                    .size(11.0),
                            );
                        });
                    }

                    Self::test_card(ui, "GPU", "Rendering pipeline stress (accelerated graphics)", &mut self.stress.gpu_on);
                    if self.stress.gpu_on {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Intensity").color(MUTED).size(11.0));
                            egui::ComboBox::from_id_salt("gpu_level")
                                .selected_text(self.stress.gpu_level.label())
                                .show_ui(ui, |ui| {
                                    for level in GpuLevel::ALL {
                                        ui.selectable_value(&mut self.stress.gpu_level, level, level.label());
                                    }
                                });
                        });
                    }
                });

                ui.add_space(8.0);
                ui.separator();

                // Live gauges.
                ui.horizontal(|ui| {
                    gauge(ui, "CPU Load", percent(self.stress.cpu_load));
                    gauge(ui, "RAM Load", percent(self.stress.ram_load));
                    gauge(ui, "CPU Temp", temp_str(self.stress.temp));
                });

                // Progress + elapsed.
                ui.horizontal(|ui| {
                    if let Some(progress) = self.stress.runner.as_ref().and_then(|r| r.progress()) {
                        ui.add(
                            egui::ProgressBar::new(progress as f32)
                                .desired_width(ui.available_width() - 60.0),
                        );
                    } else if running {
                        ui.add(egui::Spinner::new());
                    }
                    if running {
                        let secs = self
                            .stress
                            .runner
                            .as_ref()
                            .map(|r| r.elapsed_seconds())
                            .unwrap_or(0);
                        ui.label(RichText::new(format_elapsed(secs)).color(MUTED).monospace());
                    }
                });

                ui.label(RichText::new(&self.stress.status).color(MUTED).size(11.0));

                ui.add_space(6.0);
                ui.separator();

                // Controls.
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Duration (sec):").color(MUTED).size(12.0));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.stress.duration_field)
                            .desired_width(60.0)
                            .hint_text("∞"),
                    );
                    ui.label(RichText::new("(empty = run until stopped)").color(MUTED).size(11.0));

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let has_report = self.stress.reporter.sample_count() > 0;
                        let report = ui.add_enabled(!running && has_report, egui::Button::new(RichText::new("Report").color(TEXT)));
                        if report.clicked() {
                            self.export_report();
                        }
                        let stop = ui.add_enabled(
                            running,
                            egui::Button::new(RichText::new("■ Stop").color(Color32::WHITE))
                                .fill(RED),
                        );
                        if stop.clicked() {
                            if let Some(r) = &self.stress.runner {
                                r.stop("Stopped by user");
                            }
                        }
                        let start = ui.add_enabled(
                            !running,
                            egui::Button::new(RichText::new("▶ Start").color(Color32::WHITE))
                                .fill(GREEN),
                        );
                        if start.clicked() {
                            self.begin_start();
                        }
                    });
                });
            });
    }

    fn test_card(ui: &mut egui::Ui, title: &str, subtitle: &str, on: &mut bool) {
        ui.horizontal(|ui| {
            ui.checkbox(on, "");
            ui.vertical(|ui| {
                ui.label(RichText::new(title).color(TEXT).strong());
                ui.label(RichText::new(subtitle).color(MUTED).size(11.0));
            });
        });
    }

    fn begin_start(&mut self) {
        if !self.stress.any_selected() {
            self.stress.status = "Select at least one test to run.".to_string();
            return;
        }
        if self.stress.parse_duration().is_none() {
            self.stress.status = "Duration must be between 0 and 86,400 seconds.".to_string();
            return;
        }
        self.stress.confirm_open = true;
    }

    fn start_tests(&mut self) {
        let tests = self.stress.build_tests();
        let duration = self.stress.parse_duration().unwrap_or(0);
        let names = tests
            .iter()
            .map(|t| t.name())
            .collect::<Vec<_>>()
            .join(" + ");

        let runner = StressRunner::new(tests);
        if let Err(e) = runner.start(duration) {
            self.stress.status = format!("Could not start: {e}");
            return;
        }
        self.stress.reporter.start();
        self.stress.runner = Some(runner);
        self.stress.last_running = true;
        self.stress.status = format!("Starting {names} …");
    }

    fn export_report(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Export Stress Test Report")
            .set_file_name("stress-report.csv")
            .save_file()
        else {
            return;
        };
        let mut path = path;
        if path.extension().map(|e| !e.eq_ignore_ascii_case("csv")).unwrap_or(true) {
            path.set_extension("csv");
        }
        let reason = self
            .stress
            .runner
            .as_ref()
            .map(|r| r.stop_reason())
            .filter(|r| !r.trim().is_empty());
        let ok = self.stress.reporter.write_csv(&path, reason.as_deref());
        self.stress.status = if ok {
            format!("Report saved to {}", path.display())
        } else {
            format!("Could not write report to {}", path.display())
        };
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.stress.confirm_open {
            return;
        }
        let mut open = true;
        let mut confirmed = false;
        let mut close = false;
        egui::Window::new("Start Stress Test")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(RichText::new("Push your hardware to full load?").color(TEXT).strong());
                ui.add_space(6.0);
                ui.label(format!(
                    "This will run: {} at 100% load.\n\
                     Temperatures will rise. UltraMonitor will stop the test automatically\n\
                     if the CPU temperature exceeds {:.0} °C.\n\nContinue?",
                    self.stress
                        .build_tests()
                        .iter()
                        .map(|t| t.name())
                        .collect::<Vec<_>>()
                        .join(" + "),
                    AUTO_STOP_TEMP_C
                ));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button(RichText::new("Start").color(Color32::WHITE)).clicked() {
                            confirmed = true;
                        }
                        if ui.button(RichText::new("Cancel").color(TEXT)).clicked() {
                            close = true;
                        }
                    });
                });
            });
        if confirmed {
            self.stress.confirm_open = false;
            self.start_tests();
        } else if close || !open {
            self.stress.confirm_open = false;
        }
    }
}

// ---- helpers ----

fn value(readings: &[ultramonitor::monitoring::SensorReading], key: &str) -> f64 {
    readings
        .iter()
        .find(|r| r.key == key)
        .map(|r| r.value)
        .unwrap_or(f64::NAN)
}

fn toggle_row(ui: &mut egui::Ui, label: &str, on: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(if *on { TEXT } else { MUTED }).size(12.0));
        let mut v = *on;
        // A small custom switch: clickable pill.
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(40.0, 20.0), egui::Sense::click());
        let painter = ui.painter();
        let bg = if v { GREEN } else { FIELD };
        painter.rect_filled(rect, 10.0, bg);
        let knob_x = if v { rect.right() - 11.0 } else { rect.left() + 11.0 };
        painter.circle_filled(egui::pos2(knob_x, rect.center().y), 8.0, Color32::WHITE);
        if resp.clicked() {
            v = !v;
            *on = v;
        }
    });
}

fn gauge(ui: &mut egui::Ui, caption: &str, value: String) {
    egui::Frame::NONE
        .fill(PANEL2)
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(16, 8))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(caption).color(MUTED).size(10.0));
                ui.label(RichText::new(value).color(TEXT).size(16.0).strong());
            });
        });
}

fn cores_label(selected: usize, total: usize) -> String {
    if selected >= total {
        format!("All {total} threads")
    } else {
        format!("{selected} of {total} threads")
    }
}

fn percent(v: f64) -> String {
    if v.is_nan() {
        format::DASH.to_string()
    } else {
        format!("{v:.0}%")
    }
}

fn temp_str(v: f64) -> String {
    if v > 0.0 {
        format!("{v:.0} °C")
    } else {
        format::DASH.to_string()
    }
}

fn format_elapsed(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Sampling cadence: fast while a stress test runs, configured otherwise.
        let fast = self.stress.running();
        let sample_interval = if fast {
            STRESS_TICK
        } else {
            Duration::from_millis(self.interval_ms as u64)
        };
        if self.last_sample.elapsed() >= sample_interval {
            self.sample();
            if fast {
                self.stress_tick();
            }
            self.last_sample = Instant::now();
        }
        // Rebuild the System Info inventory on its own schedule, only when open.
        if self.sensor_tab == SensorTab::SystemInfo
            && self.last_system_info.elapsed() >= SYSTEM_INFO_INTERVAL
        {
            self.sample_system_info();
            self.last_system_info = Instant::now();
        }

        // Repaint at least as often as the sampling cadence.
        ctx.request_repaint_after(sample_interval);

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(BG).inner_margin(egui::Margin::same(12)))
            .show(ui, |ui| {
                self.top_bar(ui);
                ui.add_space(10.0);

                if self.show_sensors && self.show_stress {
                    ui.columns(2, |cols| {
                        self.sensors_view(&mut cols[0]);
                        self.stress_view(&mut cols[1]);
                    });
                } else if self.show_sensors {
                    self.sensors_view(ui);
                } else if self.show_stress {
                    self.stress_view(ui);
                } else {
                    self.welcome(ui);
                }
            });

        self.confirm_dialog(&ctx);
    }

    fn on_exit(&mut self) {
        if let Some(r) = &self.stress.runner {
            r.stop("Window closed");
        }
    }
}

pub fn run_gui() -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("UltraMonitor")
        .with_inner_size([1120.0, 740.0])
        .with_min_inner_size([840.0, 560.0]);

    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../resources/app-avatar.png"))
    {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "UltraMonitor",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
