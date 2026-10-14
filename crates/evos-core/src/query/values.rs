//! Parsing of function values: numbers, sizes, dates and comparisons.
//!
//! Every scalar value denotes a half-open interval: `5` is `[5, 6)`, `2024`
//! is the whole year, `today` is today from midnight to midnight. Comparison
//! operators are then defined on intervals, so `dm:>2024` means "after the
//! end of 2024" and `dm:<=2024` means "before the end of 2024".

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone};

/// A half-open interval `[lo, hi)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds<T> {
    pub lo: T,
    pub hi: T,
}

impl<T: PartialOrd + Copy> Bounds<T> {
    pub fn contains(&self, v: T) -> bool {
        self.lo <= v && v < self.hi
    }
}

/// Parses `value` as a comparison or range over intervals produced by
/// `interval`, clamped to `[min, max)`.
pub(crate) fn parse_bounds<T: Copy>(
    value: &str,
    min: T,
    max: T,
    interval: impl Fn(&str) -> Option<(T, T)>,
) -> Option<Bounds<T>> {
    if let Some((a, b)) = value.split_once("..") {
        let lo = if a.is_empty() { min } else { interval(a)?.0 };
        let hi = if b.is_empty() { max } else { interval(b)?.1 };
        return Some(Bounds { lo, hi });
    }
    let (op, rest) = ["<=", ">=", "<", ">", "="]
        .iter()
        .find_map(|op| value.strip_prefix(op).map(|r| (*op, r)))
        .unwrap_or(("=", value));
    let (s, e) = interval(rest)?;
    Some(match op {
        "<" => Bounds { lo: min, hi: s },
        "<=" => Bounds { lo: min, hi: e },
        ">" => Bounds { lo: e, hi: max },
        ">=" => Bounds { lo: s, hi: max },
        _ => Bounds { lo: s, hi: e },
    })
}

/// Parses a plain non-negative integer as the interval `[n, n + 1)`.
pub(crate) fn int_interval(s: &str) -> Option<(u64, u64)> {
    let n: u64 = s.trim().parse().ok()?;
    Some((n, n.saturating_add(1)))
}

/// Parses a size such as `512`, `10k`, `1.5mb` or `2GiB` (binary units).
pub(crate) fn size_interval(s: &str) -> Option<(u64, u64)> {
    let s = s.trim().to_ascii_lowercase();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let multiplier: u64 = match unit {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return None,
    };
    let bytes = if num.contains('.') {
        let v: f64 = num.parse().ok()?;
        (v * multiplier as f64).round() as u64
    } else {
        num.parse::<u64>().ok()?.checked_mul(multiplier)?
    };
    Some((bytes, bytes.saturating_add(1)))
}

/// Everything's named size classes.
pub(crate) fn named_size(s: &str) -> Option<Bounds<u64>> {
    const K: u64 = 1 << 10;
    const M: u64 = 1 << 20;
    let (lo, hi) = match s.to_ascii_lowercase().as_str() {
        "empty" => (0, 1),
        "tiny" => (0, 10 * K + 1),
        "small" => (10 * K + 1, 100 * K + 1),
        "medium" => (100 * K + 1, M + 1),
        "large" => (M + 1, 16 * M + 1),
        "huge" => (16 * M + 1, 128 * M + 1),
        "gigantic" => (128 * M + 1, u64::MAX),
        _ => return None,
    };
    Some(Bounds { lo, hi })
}

const NS: i64 = 1_000_000_000;

/// Parses a date keyword or literal into a `[start, end)` interval of Unix
/// nanoseconds in the local time zone.
pub(crate) fn date_interval(s: &str, now: DateTime<Local>) -> Option<(i64, i64)> {
    let s = s.trim().to_ascii_lowercase();
    let today = now.date_naive();
    let span = |a: NaiveDate, b: NaiveDate| Some((local_ns(midnight(a))?, local_ns(midnight(b))?));
    let day = |d: NaiveDate| span(d, d.succ_opt()?);
    match s.as_str() {
        "today" => return day(today),
        "yesterday" => return day(today.pred_opt()?),
        "thisweek" | "lastweek" => {
            let monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
            let start = if s == "thisweek" {
                monday
            } else {
                monday - Duration::days(7)
            };
            return span(start, start + Duration::days(7));
        }
        "thismonth" | "lastmonth" => {
            let first = today.with_day(1)?;
            let start = if s == "thismonth" {
                first
            } else {
                add_months(first, -1)?
            };
            return span(start, add_months(start, 1)?);
        }
        "thisyear" | "lastyear" => {
            let year = if s == "thisyear" {
                today.year()
            } else {
                today.year() - 1
            };
            return span(
                NaiveDate::from_ymd_opt(year, 1, 1)?,
                NaiveDate::from_ymd_opt(year + 1, 1, 1)?,
            );
        }
        _ => {}
    }
    if let Some(rel) = s.strip_prefix("last").or_else(|| s.strip_prefix("past")) {
        let split = rel.find(|c: char| !c.is_ascii_digit()).unwrap_or(rel.len());
        let (n, unit) = rel.split_at(split);
        let n: i64 = if n.is_empty() { 1 } else { n.parse().ok()? };
        let unit = unit.strip_suffix('s').unwrap_or(unit);
        let start = match unit {
            "minute" | "min" => now - Duration::minutes(n),
            "hour" => now - Duration::hours(n),
            "day" => now - Duration::days(n),
            "week" => now - Duration::weeks(n),
            "month" => now.checked_sub_months(chrono::Months::new(n as u32))?,
            "year" => now.checked_sub_months(chrono::Months::new(12 * n as u32))?,
            _ => return None,
        };
        return Some((start.timestamp_nanos_opt()?, i64::MAX));
    }
    absolute_date(&s)
}

fn midnight(d: NaiveDate) -> NaiveDateTime {
    d.and_hms_opt(0, 0, 0).expect("midnight is a valid time")
}

fn absolute_date(s: &str) -> Option<(i64, i64)> {
    let s = s.replace('/', "-");
    let parts: Vec<&str> = s.splitn(2, 't').collect();
    let date_parts: Vec<&str> = parts[0].split('-').collect();
    let year: i32 = date_parts.first()?.parse().ok()?;
    if !(1000..=9999).contains(&year) {
        return None;
    }
    let month: Option<u32> = date_parts.get(1).map(|m| m.parse()).transpose().ok()?;
    let dom: Option<u32> = date_parts.get(2).map(|d| d.parse()).transpose().ok()?;
    if date_parts.len() > 3 {
        return None;
    }
    let (start, end) = match (month, dom) {
        (None, _) => (
            midnight(NaiveDate::from_ymd_opt(year, 1, 1)?),
            midnight(NaiveDate::from_ymd_opt(year + 1, 1, 1)?),
        ),
        (Some(m), None) => {
            let first = NaiveDate::from_ymd_opt(year, m, 1)?;
            (midnight(first), midnight(add_months(first, 1)?))
        }
        (Some(m), Some(d)) => {
            let date = NaiveDate::from_ymd_opt(year, m, d)?;
            match parts.get(1) {
                None => (midnight(date), midnight(date.succ_opt()?)),
                Some(time) => time_interval(date, time)?,
            }
        }
    };
    Some((local_ns(start)?, local_ns(end)?))
}

fn time_interval(date: NaiveDate, time: &str) -> Option<(NaiveDateTime, NaiveDateTime)> {
    let fields: Vec<u32> = time
        .split(':')
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    let (start, step) = match fields.as_slice() {
        [h] => (date.and_hms_opt(*h, 0, 0)?, Duration::hours(1)),
        [h, m] => (date.and_hms_opt(*h, *m, 0)?, Duration::minutes(1)),
        [h, m, s] => (date.and_hms_opt(*h, *m, *s)?, Duration::seconds(1)),
        _ => return None,
    };
    Some((start, start + step))
}

fn add_months(date: NaiveDate, months: i32) -> Option<NaiveDate> {
    let total = date.year() * 12 + date.month0() as i32 + months;
    NaiveDate::from_ymd_opt(total.div_euclid(12), total.rem_euclid(12) as u32 + 1, 1)
}

/// Converts a local wall-clock time to Unix nanoseconds, resolving DST gaps
/// and overlaps to the earliest valid instant.
fn local_ns(t: NaiveDateTime) -> Option<i64> {
    let local = Local.from_local_datetime(&t);
    let resolved = local.earliest().or_else(|| {
        Local
            .from_local_datetime(&(t + Duration::hours(1)))
            .earliest()
    })?;
    resolved.timestamp().checked_mul(NS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, 0, 0).earliest().unwrap()
    }

    fn ns(t: DateTime<Local>) -> i64 {
        t.timestamp_nanos_opt().unwrap()
    }

    #[test]
    fn sizes() {
        assert_eq!(size_interval("512"), Some((512, 513)));
        assert_eq!(size_interval("10k"), Some((10240, 10241)));
        assert_eq!(size_interval("1.5MB"), Some((1572864, 1572865)));
        assert_eq!(size_interval("2GiB"), Some((2 << 30, (2 << 30) + 1)));
        assert_eq!(size_interval("abc"), None);
        assert_eq!(size_interval("5xb"), None);
    }

    #[test]
    fn comparisons_on_intervals() {
        let b = |v| parse_bounds(v, 0u64, u64::MAX, size_interval).unwrap();
        assert_eq!(
            b(">1k"),
            Bounds {
                lo: 1025,
                hi: u64::MAX
            }
        );
        assert_eq!(
            b(">=1k"),
            Bounds {
                lo: 1024,
                hi: u64::MAX
            }
        );
        assert_eq!(b("<1k"), Bounds { lo: 0, hi: 1024 });
        assert_eq!(b("<=1k"), Bounds { lo: 0, hi: 1025 });
        assert_eq!(b("1k"), Bounds { lo: 1024, hi: 1025 });
        assert_eq!(b("1k..2k"), Bounds { lo: 1024, hi: 2049 });
        assert_eq!(b("..2k"), Bounds { lo: 0, hi: 2049 });
        assert!(parse_bounds(">x", 0u64, u64::MAX, size_interval).is_none());
    }

    #[test]
    fn named_sizes() {
        assert!(named_size("empty").unwrap().contains(0));
        assert!(named_size("tiny").unwrap().contains(10 * 1024));
        assert!(named_size("gigantic").unwrap().contains(1 << 40));
        assert!(named_size("nope").is_none());
    }

    #[test]
    fn day_keywords() {
        let now = at(2024, 3, 13, 15); // a Wednesday
        let (s, e) = date_interval("today", now).unwrap();
        assert_eq!(s, ns(at(2024, 3, 13, 0)));
        assert_eq!(e, ns(at(2024, 3, 14, 0)));
        let (s, _) = date_interval("yesterday", now).unwrap();
        assert_eq!(s, ns(at(2024, 3, 12, 0)));
        let (s, e) = date_interval("thisweek", now).unwrap();
        assert_eq!((s, e), (ns(at(2024, 3, 11, 0)), ns(at(2024, 3, 18, 0))));
        let (s, _) = date_interval("lastweek", now).unwrap();
        assert_eq!(s, ns(at(2024, 3, 4, 0)));
        let (s, e) = date_interval("lastmonth", now).unwrap();
        assert_eq!((s, e), (ns(at(2024, 2, 1, 0)), ns(at(2024, 3, 1, 0))));
        let (s, _) = date_interval("lastyear", now).unwrap();
        assert_eq!(s, ns(at(2023, 1, 1, 0)));
    }

    #[test]
    fn relative_ranges() {
        let now = at(2024, 3, 13, 15);
        let (s, e) = date_interval("last2days", now).unwrap();
        assert_eq!((s, e), (ns(at(2024, 3, 11, 15)), i64::MAX));
        let (s, _) = date_interval("pasthour", now).unwrap();
        assert_eq!(s, ns(at(2024, 3, 13, 14)));
        assert!(date_interval("last3fortnights", now).is_none());
    }

    #[test]
    fn absolute_dates() {
        let now = at(2024, 3, 13, 15);
        let (s, e) = date_interval("2023", now).unwrap();
        assert_eq!((s, e), (ns(at(2023, 1, 1, 0)), ns(at(2024, 1, 1, 0))));
        let (s, e) = date_interval("2023-12", now).unwrap();
        assert_eq!((s, e), (ns(at(2023, 12, 1, 0)), ns(at(2024, 1, 1, 0))));
        let (s, e) = date_interval("2023/02/28", now).unwrap();
        assert_eq!((s, e), (ns(at(2023, 2, 28, 0)), ns(at(2023, 3, 1, 0))));
        let (s, e) = date_interval("2023-02-28T10", now).unwrap();
        assert_eq!((s, e), (ns(at(2023, 2, 28, 10)), ns(at(2023, 2, 28, 11))));
        assert!(date_interval("2023-13", now).is_none());
        assert!(date_interval("99", now).is_none());
    }
}
