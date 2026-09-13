//! Small formatting helpers shared by the sensors table and the System Info
//! tab. Unavailable values always render as the em dash `—`.

pub const DASH: &str = "—";

/// Human-readable bytes: "1.2 KB", "4.5 GB", …
pub fn bytes(value: u64) -> String {
    if value == 0 {
        return "0 B".to_string();
    }
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = value as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", units[i])
}

/// Frequency in hertz → "3.60 GHz".
pub fn ghz(hz: f64) -> String {
    if hz <= 0.0 || hz.is_nan() {
        DASH.to_string()
    } else {
        format!("{:.2} GHz", hz / 1e9)
    }
}

/// MB/s rate → "12.30 MB/s".
pub fn rate(mbps: f64) -> String {
    if mbps.is_nan() || mbps < 0.0 {
        DASH.to_string()
    } else {
        format!("{mbps:.2} MB/s")
    }
}

/// Link speed in bits/s → "1.0 Gbps" / "100 Mbps".
pub fn bits(bits_per_sec: u64) -> String {
    if bits_per_sec == 0 {
        return DASH.to_string();
    }
    if bits_per_sec >= 1_000_000_000 {
        format!("{:.1} Gbps", bits_per_sec as f64 / 1e9)
    } else {
        format!("{:.0} Mbps", bits_per_sec as f64 / 1e6)
    }
}

/// Percent, tolerating NaN / negative "unknown" values.
pub fn percent(value: f64) -> String {
    if value.is_nan() || value < 0.0 {
        DASH.to_string()
    } else {
        format!("{value:.1}%")
    }
}

/// Load average (or any small scalar).
pub fn load_avg(value: f64) -> String {
    if value.is_nan() {
        DASH.to_string()
    } else {
        format!("{value:.2}")
    }
}

/// Duration in seconds → "3d 4h 5m" / "45m 30s".
pub fn time(seconds: i64) -> String {
    if seconds < 0 {
        return DASH.to_string();
    }
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

/// Battery minutes (input in seconds; negative = unknown/calculating).
pub fn minutes(seconds: f64) -> String {
    if seconds < 0.0 {
        return DASH.to_string();
    }
    let total = (seconds / 60.0) as i64;
    if total >= 60 {
        format!("{}h {:02}m", total / 60, total % 60)
    } else {
        format!("{total} min")
    }
}

/// Battery capacity in milliwatt-hours → "45.2 Wh".
pub fn watt_hours(mwh: i64) -> String {
    if mwh <= 0 {
        DASH.to_string()
    } else {
        format!("{:.1} Wh", mwh as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_units() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1024), "1.0 KB");
        assert_eq!(bytes(4 * 1024 * 1024 * 1024), "4.0 GB");
    }

    #[test]
    fn time_formats() {
        assert_eq!(time(0), "0s");
        assert_eq!(time(65), "1m 5s");
        assert_eq!(time(3661), "1h 1m");
        assert_eq!(time(3 * 86400 + 5), "3d 0h 0m");
    }

    #[test]
    fn dash_for_unavailable() {
        assert_eq!(percent(f64::NAN), "—");
        assert_eq!(ghz(0.0), "—");
        assert_eq!(rate(f64::NAN), "—");
        assert_eq!(bits(0), "—");
    }
}
