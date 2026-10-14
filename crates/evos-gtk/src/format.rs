//! Display formatting for result rows.

use chrono::{Local, TimeZone};

/// `1.5 MiB` style sizes.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Local date and time of a Unix-nanosecond timestamp.
pub fn date(ns: i64) -> String {
    Local
        .timestamp_opt(
            ns.div_euclid(1_000_000_000),
            ns.rem_euclid(1_000_000_000) as u32,
        )
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// `12,345,678`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(3 << 20), "3.0 MiB");
        assert_eq!(count(0), "0");
        assert_eq!(count(1234567), "1,234,567");
        assert_eq!(count(100), "100");
        assert_eq!(date(0).len(), 16);
    }
}
