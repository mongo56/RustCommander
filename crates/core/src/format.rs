use chrono::{DateTime, Local};

/// Compact human-readable size, e.g. `912 B`, `14.2 KB`, `3.1 GB`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn date(dt: &Option<DateTime<Local>>) -> String {
    dt.map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes() {
        assert_eq!(super::size(0), "0 B");
        assert_eq!(super::size(1023), "1023 B");
        assert_eq!(super::size(1536), "1.5 KB");
        assert_eq!(super::size(500 * 1024 * 1024), "500 MB");
    }
}
