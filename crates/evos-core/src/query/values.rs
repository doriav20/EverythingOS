//! Parsing of function values: numbers, sizes, dates, durations and
//! comparisons.
//!
//! Every scalar value denotes a half-open interval: `5` is `[5, 6)`, `2024`
//! is the whole year, `today` is today from midnight to midnight. Comparison
//! operators are then defined on intervals, so `dm:>2024` means "after the
//! end of 2024" and `dm:<=2024` means "before the end of 2024".

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Weekday};

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
/// `interval`, clamped to `[min, max)`. With `dash_ranges`, `a-b` is a
/// range like `a..b` (not for dates, which contain dashes).
pub(crate) fn parse_bounds<T: Copy>(
    value: &str,
    min: T,
    max: T,
    dash_ranges: bool,
    interval: impl Fn(&str) -> Option<(T, T)>,
) -> Option<Bounds<T>> {
    let range = |a: &str, b: &str| -> Option<Bounds<T>> {
        let lo = if a.is_empty() { min } else { interval(a)?.0 };
        let hi = if b.is_empty() { max } else { interval(b)?.1 };
        Some(Bounds { lo, hi })
    };
    if let Some((a, b)) = value.split_once("..") {
        return range(a, b);
    }
    if dash_ranges {
        if let Some((a, b)) = value.split_once('-') {
            if let Some(r) = range(a, b) {
                return Some(r);
            }
        }
    }
    let (op, rest) = ["<=", ">=", "==", "<", ">", "="]
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
    if num.is_empty() {
        return None;
    }
    let multiplier: u64 = match unit {
        "" | "b" | "bytes" => 1,
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

const MS_MINUTE: u64 = 60_000;

/// Everything's named durations, in milliseconds.
pub(crate) fn named_duration(s: &str) -> Option<Bounds<u64>> {
    let (lo, hi) = match s.to_ascii_lowercase().as_str() {
        "very-short" | "veryshort" => (0, MS_MINUTE),
        "short" => (MS_MINUTE, 5 * MS_MINUTE),
        "medium" => (5 * MS_MINUTE, 30 * MS_MINUTE),
        "long" => (30 * MS_MINUTE, 60 * MS_MINUTE),
        "very-long" | "verylong" => (60 * MS_MINUTE, u64::MAX),
        _ => return None,
    };
    Some(Bounds { lo, hi })
}

/// Parses a duration into an interval of milliseconds:
/// `[[[DD:]HH:]MM:]SS[.sss]`, `1h30m`, `10mins`, `PT1H30M`, `P2W`.
pub(crate) fn duration_interval(s: &str) -> Option<(u64, u64)> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    if let Some(iso) = s.strip_prefix('p') {
        return iso_duration(iso);
    }
    if s.chars()
        .all(|c| c.is_ascii_digit() || c == ':' || c == '.')
    {
        let (whole, frac) = match s.split_once('.') {
            Some((w, f)) => (w, Some(f)),
            None => (s.as_str(), None),
        };
        let fields: Vec<u64> = whole
            .split(':')
            .map(|f| f.parse().ok())
            .collect::<Option<_>>()?;
        if fields.len() > 4 {
            return None;
        }
        let units = [86_400_000, 3_600_000, 60_000, 1000];
        let mut ms = 0u64;
        for (v, unit) in fields.iter().rev().zip(units.iter().rev()) {
            ms = ms.checked_add(v.checked_mul(*unit)?)?;
        }
        let step = match frac {
            Some(f) => {
                let digits = f.len().min(3) as u32;
                let value: u64 = f.get(..digits as usize)?.parse().ok()?;
                ms += value * 10u64.pow(3 - digits);
                10u64.pow(3 - digits)
            }
            None => 1000,
        };
        return Some((ms, ms + step));
    }
    // Unit groups: 1h30m, 10mins, 2 days.
    let mut ms = 0u64;
    let mut step = u64::MAX;
    let mut rest = s.as_str();
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if digits == 0 {
            return None;
        }
        let value: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(rest.len());
        let unit = match &rest[..unit_len] {
            "d" | "day" | "days" => 86_400_000,
            "h" | "hour" | "hours" | "hr" | "hrs" => 3_600_000,
            "m" | "min" | "mins" | "minute" | "minutes" => 60_000,
            "s" | "sec" | "secs" | "second" | "seconds" | "" => 1000,
            "ms" => 1,
            _ => return None,
        };
        rest = &rest[unit_len..];
        ms += (value * unit as f64) as u64;
        step = step.min(unit);
    }
    Some((ms, ms + step))
}

fn iso_duration(s: &str) -> Option<(u64, u64)> {
    let mut ms = 0u64;
    let mut step = u64::MAX;
    let mut in_time = false;
    let mut num = String::new();
    for c in s.chars() {
        match c {
            't' => in_time = true,
            '0'..='9' | '.' => num.push(c),
            _ => {
                let v: f64 = num.parse().ok()?;
                num.clear();
                let unit: u64 = match (c, in_time) {
                    ('y', false) => 365 * 86_400_000,
                    ('m', false) => 30 * 86_400_000,
                    ('w', false) => 7 * 86_400_000,
                    ('d', false) => 86_400_000,
                    ('h', true) => 3_600_000,
                    ('m', true) => 60_000,
                    ('s', true) => 1000,
                    _ => return None,
                };
                ms += (v * unit as f64) as u64;
                step = step.min(unit);
            }
        }
    }
    (num.is_empty() && step != u64::MAX).then_some((ms, ms + step))
}

const NS: i64 = 1_000_000_000;

/// Interval used for `unknown` dates (stored as 0).
pub const UNKNOWN_DATE: (i64, i64) = (0, 1);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Period {
    Year,
    Month,
    Week,
    Day,
}

fn period(word: &str) -> Option<Period> {
    Some(match word.trim_end_matches('s') {
        "year" => Period::Year,
        "month" => Period::Month,
        "week" => Period::Week,
        "day" => Period::Day,
        _ => return None,
    })
}

fn month_of(word: &str) -> Option<u32> {
    const NAMES: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    NAMES
        .iter()
        .position(|n| *n == word || (word.len() == 3 && n.starts_with(word)))
        .map(|i| i as u32 + 1)
}

fn weekday_of(word: &str) -> Option<Weekday> {
    const NAMES: [(&str, Weekday); 7] = [
        ("monday", Weekday::Mon),
        ("tuesday", Weekday::Tue),
        ("wednesday", Weekday::Wed),
        ("thursday", Weekday::Thu),
        ("friday", Weekday::Fri),
        ("saturday", Weekday::Sat),
        ("sunday", Weekday::Sun),
    ];
    NAMES
        .iter()
        .find(|(n, _)| *n == word || (word.len() == 3 && n.starts_with(word)))
        .map(|(_, d)| *d)
}

/// The calendar period containing `day`, shifted by `offset` periods.
fn calendar(day: NaiveDate, p: Period, offset: i32) -> Option<(NaiveDate, NaiveDate)> {
    Some(match p {
        Period::Day => {
            let d = day + Duration::days(offset as i64);
            (d, d.succ_opt()?)
        }
        Period::Week => {
            let monday = day - Duration::days(day.weekday().num_days_from_monday() as i64)
                + Duration::weeks(offset as i64);
            (monday, monday + Duration::days(7))
        }
        Period::Month => {
            let first = add_months(day.with_day(1)?, offset)?;
            (first, add_months(first, 1)?)
        }
        Period::Year => {
            let year = day.year() + offset;
            (
                NaiveDate::from_ymd_opt(year, 1, 1)?,
                NaiveDate::from_ymd_opt(year + 1, 1, 1)?,
            )
        }
    })
}

/// Splits `last2weeks` into (2, "weeks"); the number defaults to 1.
fn count_and_unit(s: &str) -> Option<(i64, &str)> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n = if n.is_empty() { 1 } else { n.parse().ok()? };
    (!unit.is_empty()).then_some((n, unit))
}

fn shift(now: DateTime<Local>, n: i64, unit: &str) -> Option<DateTime<Local>> {
    let unit = unit.trim_end_matches('s');
    let months = |m: i64| chrono::Months::new(m.unsigned_abs() as u32);
    Some(match unit {
        "sec" | "second" => now + Duration::seconds(n),
        "min" | "minute" => now + Duration::minutes(n),
        "hour" => now + Duration::hours(n),
        "day" => now + Duration::days(n),
        "week" => now + Duration::weeks(n),
        "month" if n < 0 => now.checked_sub_months(months(n))?,
        "month" => now.checked_add_months(months(n))?,
        "year" if n < 0 => now.checked_sub_months(months(12 * n))?,
        "year" => now.checked_add_months(months(12 * n))?,
        _ => return None,
    })
}

/// Parses a date keyword or literal into a `[start, end)` interval of Unix
/// nanoseconds in the local time zone.
pub(crate) fn date_interval(s: &str, now: DateTime<Local>) -> Option<(i64, i64)> {
    let s = s.trim().to_ascii_lowercase().replace([' ', '_'], "");
    let today = now.date_naive();
    let span =
        |(a, b): (NaiveDate, NaiveDate)| Some((local_ns(midnight(a))?, local_ns(midnight(b))?));
    let ns = |t: DateTime<Local>| t.timestamp_nanos_opt();

    match s.as_str() {
        "today" => return span(calendar(today, Period::Day, 0)?),
        "yesterday" => return span(calendar(today, Period::Day, -1)?),
        "tomorrow" => return span(calendar(today, Period::Day, 1)?),
        "unknown" => return Some(UNKNOWN_DATE),
        "mtd" => return Some((span(calendar(today, Period::Month, 0)?)?.0, ns(now)? + 1)),
        "ytd" => return Some((span(calendar(today, Period::Year, 0)?)?.0, ns(now)? + 1)),
        _ => {}
    }
    if let Some(m) = month_of(&s) {
        let first = NaiveDate::from_ymd_opt(today.year(), m, 1)?;
        return span((first, add_months(first, 1)?));
    }
    if let Some(wd) = weekday_of(&s) {
        // The most recent such day, today included.
        let back = (today.weekday().num_days_from_monday() + 7 - wd.num_days_from_monday()) % 7;
        return span(calendar(today, Period::Day, -(back as i32))?);
    }
    for (prefix, offset) in [
        ("this", 0),
        ("current", 0),
        ("last", -1),
        ("coming", 1),
        ("next", 1),
    ] {
        if let Some(p) = s.strip_prefix(prefix).and_then(period) {
            return span(calendar(today, p, offset)?);
        }
    }
    for prefix in ["previous", "prev"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let (n, unit) = count_and_unit(rest)?;
            let p = period(unit)?;
            let (start, _) = calendar(today, p, -(n as i32))?;
            let (end, _) = calendar(today, p, 0)?;
            return span((start, end));
        }
    }
    // Rolling windows: last2days, past3hours, next2weeks.
    for (prefix, sign) in [("last", -1i64), ("past", -1), ("coming", 1), ("next", 1)] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let (n, unit) = count_and_unit(rest)?;
            let other = shift(now, sign * n, unit)?;
            return Some(if sign < 0 {
                (ns(other)?, i64::MAX)
            } else {
                (ns(now)?, ns(other)?)
            });
        }
    }
    absolute_date(&s)
}

fn midnight(d: NaiveDate) -> NaiveDateTime {
    d.and_hms_opt(0, 0, 0).expect("midnight is a valid time")
}

fn absolute_date(s: &str) -> Option<(i64, i64)> {
    // YYYYMMDD[Thhmmss]
    let compact = s.split('t').next().unwrap_or("");
    if compact.len() == 8 && compact.chars().all(|c| c.is_ascii_digit()) {
        let (y, m, d) = (&compact[..4], &compact[4..6], &compact[6..]);
        let rest = s.get(8..).unwrap_or("");
        let time = rest.strip_prefix('t').map(|t| {
            t.as_bytes()
                .chunks(2)
                .map(|c| std::str::from_utf8(c).unwrap_or(""))
                .collect::<Vec<_>>()
                .join(":")
        });
        let iso = match time {
            Some(t) => format!("{y}-{m}-{d}t{t}"),
            None => format!("{y}-{m}-{d}"),
        };
        return absolute_date(&iso);
    }
    let s = s.replace('/', "-");
    let parts: Vec<&str> = s.splitn(2, 't').collect();
    let mut fields: Vec<&str> = parts[0].split('-').collect();
    // day-month-year and month-year orders: the year is the 4-digit field.
    if fields.len() >= 2 && fields.last().is_some_and(|y| y.len() == 4) && fields[0].len() <= 2 {
        fields.reverse();
        if fields.len() == 3 {
            // Now year, month, day for d/m/y; swap for m/d/y when the
            // "month" cannot be one.
            let (m, d): (u32, u32) = (fields[1].parse().ok()?, fields[2].parse().ok()?);
            if m > 12 && d <= 12 {
                fields.swap(1, 2);
            }
        }
    }
    let year: i32 = fields.first()?.parse().ok()?;
    if !(1000..=9999).contains(&year) {
        return None;
    }
    let month: Option<u32> = fields.get(1).map(|m| m.parse()).transpose().ok()?;
    let dom: Option<u32> = fields.get(2).map(|d| d.parse()).transpose().ok()?;
    if fields.len() > 3 {
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

    fn days(now: DateTime<Local>, s: &str) -> (i64, i64) {
        date_interval(s, now).unwrap_or_else(|| panic!("{s} did not parse"))
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
        let b = |v| parse_bounds(v, 0u64, u64::MAX, true, size_interval).unwrap();
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
        assert_eq!(b("==1k"), Bounds { lo: 1024, hi: 1025 });
        assert_eq!(b("1k..2k"), Bounds { lo: 1024, hi: 2049 });
        assert_eq!(
            b("1k-2k"),
            Bounds { lo: 1024, hi: 2049 },
            "start-end ranges"
        );
        assert_eq!(b("..2k"), Bounds { lo: 0, hi: 2049 });
        assert!(parse_bounds(">x", 0u64, u64::MAX, true, size_interval).is_none());
    }

    #[test]
    fn named_sizes_and_durations() {
        assert!(named_size("empty").unwrap().contains(0));
        assert!(named_size("tiny").unwrap().contains(10 * 1024));
        assert!(named_size("gigantic").unwrap().contains(1 << 40));
        assert!(named_size("nope").is_none());
        assert!(named_duration("short").unwrap().contains(2 * 60_000));
        assert!(named_duration("very-long").unwrap().contains(2 * 3_600_000));
    }

    #[test]
    fn durations() {
        assert_eq!(duration_interval("90"), Some((90_000, 91_000)));
        assert_eq!(duration_interval("1:30"), Some((90_000, 91_000)));
        assert_eq!(duration_interval("1:00:00"), Some((3_600_000, 3_601_000)));
        assert_eq!(duration_interval("1:02.5"), Some((62_500, 62_600)));
        assert_eq!(duration_interval("1hour"), Some((3_600_000, 7_200_000)));
        assert_eq!(duration_interval("10mins"), Some((600_000, 660_000)));
        assert_eq!(duration_interval("1h30m"), Some((5_400_000, 5_460_000)));
        assert_eq!(duration_interval("PT1H30M"), Some((5_400_000, 5_460_000)));
        assert_eq!(duration_interval("P2W").map(|d| d.0), Some(14 * 86_400_000));
        assert!(duration_interval("soon").is_none());
    }

    #[test]
    fn day_keywords() {
        let now = at(2024, 3, 13, 15); // a Wednesday
        assert_eq!(
            days(now, "today"),
            (ns(at(2024, 3, 13, 0)), ns(at(2024, 3, 14, 0)))
        );
        assert_eq!(days(now, "yesterday").0, ns(at(2024, 3, 12, 0)));
        assert_eq!(
            days(now, "thisweek"),
            (ns(at(2024, 3, 11, 0)), ns(at(2024, 3, 18, 0)))
        );
        assert_eq!(days(now, "currentweek"), days(now, "thisweek"));
        assert_eq!(days(now, "lastweek").0, ns(at(2024, 3, 4, 0)));
        assert_eq!(days(now, "nextweek").0, ns(at(2024, 3, 18, 0)));
        assert_eq!(
            days(now, "lastmonth"),
            (ns(at(2024, 2, 1, 0)), ns(at(2024, 3, 1, 0)))
        );
        assert_eq!(days(now, "comingmonth").0, ns(at(2024, 4, 1, 0)));
        assert_eq!(days(now, "lastyear").0, ns(at(2023, 1, 1, 0)));
        assert_eq!(
            days(now, "prev2weeks"),
            (ns(at(2024, 2, 26, 0)), ns(at(2024, 3, 11, 0)))
        );
        assert_eq!(days(now, "previous2weeks"), days(now, "prev2weeks"));
    }

    #[test]
    fn named_months_and_weekdays() {
        let now = at(2024, 3, 13, 15); // a Wednesday
        assert_eq!(
            days(now, "january"),
            (ns(at(2024, 1, 1, 0)), ns(at(2024, 2, 1, 0)))
        );
        assert_eq!(days(now, "dec").0, ns(at(2024, 12, 1, 0)));
        assert_eq!(days(now, "monday").0, ns(at(2024, 3, 11, 0)));
        assert_eq!(days(now, "wed").0, ns(at(2024, 3, 13, 0)), "today counts");
        assert_eq!(days(now, "thursday").0, ns(at(2024, 3, 7, 0)));
        assert_eq!(days(now, "mtd"), (ns(at(2024, 3, 1, 0)), ns(now) + 1));
        assert_eq!(days(now, "ytd").0, ns(at(2024, 1, 1, 0)));
        assert_eq!(days(now, "unknown"), UNKNOWN_DATE);
    }

    #[test]
    fn relative_ranges() {
        let now = at(2024, 3, 13, 15);
        assert_eq!(days(now, "last2days"), (ns(at(2024, 3, 11, 15)), i64::MAX));
        assert_eq!(days(now, "pasthour").0, ns(at(2024, 3, 13, 14)));
        assert_eq!(days(now, "next3days"), (ns(now), ns(at(2024, 3, 16, 15))));
        assert!(date_interval("last3fortnights", now).is_none());
    }

    #[test]
    fn absolute_dates() {
        let now = at(2024, 3, 13, 15);
        assert_eq!(
            days(now, "2023"),
            (ns(at(2023, 1, 1, 0)), ns(at(2024, 1, 1, 0)))
        );
        assert_eq!(
            days(now, "2023-12"),
            (ns(at(2023, 12, 1, 0)), ns(at(2024, 1, 1, 0)))
        );
        assert_eq!(
            days(now, "2023/02/28"),
            (ns(at(2023, 2, 28, 0)), ns(at(2023, 3, 1, 0)))
        );
        assert_eq!(
            days(now, "2023-02-28T10"),
            (ns(at(2023, 2, 28, 10)), ns(at(2023, 2, 28, 11)))
        );
        assert_eq!(days(now, "20230228"), days(now, "2023-02-28"));
        assert_eq!(
            days(now, "20230228T1015").0,
            ns(at(2023, 2, 28, 10)) + 15 * 60 * NS
        );
        assert_eq!(
            days(now, "28/02/2023"),
            days(now, "2023-02-28"),
            "day/month/year"
        );
        assert_eq!(
            days(now, "02/28/2023"),
            days(now, "2023-02-28"),
            "month/day/year"
        );
        assert_eq!(days(now, "12/2023"), days(now, "2023-12"));
        assert!(date_interval("2023-13", now).is_none());
        assert!(date_interval("99", now).is_none());
    }
}
