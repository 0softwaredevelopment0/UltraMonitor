//! Portable application configuration persisted as `config.json` next to the
//! executable (falling back to `~/.ultramonitor` for `cargo run` dev builds,
//! so developer runs never write into `target/`). Currently stores the sensor
//! refresh interval in milliseconds.

use std::path::PathBuf;

pub const DEFAULT_INTERVAL_MS: i64 = 1000;
pub const MIN_INTERVAL_MS: i64 = 1;
pub const MAX_INTERVAL_MS: i64 = 10_000;

const FILE_NAME: &str = "config.json";

/// Directory next to the executable when packaged, otherwise `~/.ultramonitor`.
pub fn app_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // A `cargo run`/`cargo test` binary lives under `target/...`; treat
            // that as a dev run and use a per-user directory instead.
            let is_dev = dir
                .components()
                .any(|c| c.as_os_str() == "target");
            if !is_dev {
                return dir.to_path_buf();
            }
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".ultramonitor")
}

pub fn config_file() -> PathBuf {
    app_dir().join(FILE_NAME)
}

/// Loads the refresh interval, clamped to the valid range; default on error.
pub fn load_interval_ms() -> i64 {
    let file = config_file();
    if let Ok(content) = std::fs::read_to_string(&file) {
        let digits: String = content.chars().filter(|c| c.is_ascii_digit()).collect();
        if let Ok(value) = digits.parse::<i64>() {
            if (MIN_INTERVAL_MS..=MAX_INTERVAL_MS).contains(&value) {
                return value;
            }
        }
    }
    DEFAULT_INTERVAL_MS
}

/// Persists the interval (clamped); returns `true` on success. Writes atomically
/// via a temp sibling + rename so a crash never leaves a truncated file.
pub fn save_interval_ms(interval_ms: i64) -> bool {
    let clamped = interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS);
    let dir = app_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let target = config_file();
    let temp = dir.join(format!("{}.tmp", FILE_NAME));
    let json = format!("{{\"refreshIntervalMs\": {clamped}}}\n");
    if std::fs::write(&temp, json).is_err() {
        return false;
    }
    std::fs::rename(&temp, &target).is_ok()
}
