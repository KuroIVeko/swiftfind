//! Search filters: `ext:`, `size:`, `dm:`, `file:` and `folder:`.
//!
//! Syntax (case-insensitive, Everything-like):
//! - `ext:txt;md` — extension is one of the list
//! - `size:>10mb`, `size:<=1.5k`, `size:1m..20m`, `size:large`
//!   (units b/k/kb/m/mb/g/gb/t/tb, 1024-based; a bare number is bytes)
//! - `dm:today`, `dm:thisweek`, `dm:2024`, `dm:2024-05`, `dm:>=2024-05-01`,
//!   `dm:2024-01..2024-03` (dates are local time)
//! - `file:` / `folder:` — only files / only folders

use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};
use std::ops::RangeInclusive;

pub enum Filter {
    /// Lower-case extensions without the dot.
    Ext(Vec<String>),
    /// Byte range of files with a known size.
    Size(RangeInclusive<u64>),
    /// Unix-seconds range of entries with a known modification time.
    Modified(RangeInclusive<u64>),
    /// `true`: folders only; `false`: files only.
    Dir(bool),
}

/// What a query token turned out to be.
pub enum Token {
    /// Not a filter: plain text to match.
    Text,
    /// A filter, plus text that must also match (`file:abc`).
    Filter(Filter, Option<String>),
    /// A filter keyword with nothing after it yet (`size:`); ignored.
    Incomplete,
    Error(String),
}

/// Parses a lower-cased query token. `today` is the local date "now".
pub fn parse(token: &str, today: NaiveDate) -> Token {
    let Some((key, value)) = token.split_once(':') else {
        return Token::Text;
    };
    match key {
        "ext" => {
            let exts: Vec<String> = value
                .split([';', ','])
                .map(|e| e.trim_start_matches('.'))
                .filter(|e| !e.is_empty())
                .map(Into::into)
                .collect();
            if exts.is_empty() {
                Token::Incomplete
            } else {
                Token::Filter(Filter::Ext(exts), None)
            }
        }
        "size" if value.is_empty() => Token::Incomplete,
        "size" => match size_range(value) {
            Some(r) => Token::Filter(Filter::Size(r), None),
            None => Token::Error(format!(
                "无法识别 size:{value}（示例：size:>10mb、size:1m..20m）"
            )),
        },
        "dm" | "datemodified" if value.is_empty() => Token::Incomplete,
        "dm" | "datemodified" => match date_range(value, today) {
            Some(r) => Token::Filter(Filter::Modified(r), None),
            None => Token::Error(format!(
                "无法识别 {key}:{value}（示例：dm:today、dm:2024-05、dm:>=2024-05-01）"
            )),
        },
        "file" | "folder" => {
            let text = (!value.is_empty()).then(|| value.to_string());
            Token::Filter(Filter::Dir(key == "folder"), text)
        }
        _ => Token::Text,
    }
}

/// Splits a comparison prefix: returns (op, rest). `..` ranges are handled by callers.
fn comparison(v: &str) -> (&'static str, &str) {
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = v.strip_prefix(op) {
            return (op, rest);
        }
    }
    ("=", v)
}

/// Turns a value's own span `[lo, hi]` and a comparison into a range.
fn compare(op: &str, lo: u64, hi: u64) -> Option<RangeInclusive<u64>> {
    Some(match op {
        ">" => hi.checked_add(1)?..=u64::MAX,
        ">=" => lo..=u64::MAX,
        "<" => 0..=lo.checked_sub(1)?,
        "<=" => 0..=hi,
        _ => lo..=hi,
    })
}

fn size_range(v: &str) -> Option<RangeInclusive<u64>> {
    const K: u64 = 1024;
    const M: u64 = K * K;
    let named = match v {
        "empty" => Some(0..=0),
        "tiny" => Some(1..=10 * K),
        "small" => Some(10 * K + 1..=100 * K),
        "medium" => Some(100 * K + 1..=M),
        "large" => Some(M + 1..=16 * M),
        "huge" => Some(16 * M + 1..=128 * M),
        "gigantic" => Some(128 * M + 1..=u64::MAX),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if let Some((a, b)) = v.split_once("..") {
        let (a, b) = (bytes(a)?, bytes(b)?);
        return (a <= b).then_some(a..=b);
    }
    let (op, rest) = comparison(v);
    let n = bytes(rest)?;
    compare(op, n, n)
}

/// "1.5mb" → bytes.
fn bytes(s: &str) -> Option<u64> {
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let scale: u64 = match unit {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        "t" | "tb" => 1 << 40,
        _ => return None,
    };
    let n: f64 = num
        .parse()
        .ok()
        .filter(|n: &f64| n.is_finite() && *n >= 0.0)?;
    let b = n * scale as f64;
    (b < u64::MAX as f64).then(|| b.round() as u64)
}

fn date_range(v: &str, today: NaiveDate) -> Option<RangeInclusive<u64>> {
    if let Some((a, b)) = v.split_once("..") {
        let (a, _) = day_span(a, today)?;
        let (_, b) = day_span(b, today)?;
        return (a < b).then(|| secs(a)..=secs(b) - 1);
    }
    let (op, rest) = comparison(v);
    let (start, end) = day_span(rest, today)?;
    compare(op, secs(start), secs(end) - 1)
}

/// Local midnight of `d` as Unix seconds (clamped at 0).
fn secs(d: NaiveDate) -> u64 {
    let midnight = d.and_hms_opt(0, 0, 0).unwrap();
    // A DST gap at midnight: take the first valid instant after it.
    let t = Local
        .from_local_datetime(&midnight)
        .earliest()
        .or_else(|| {
            Local
                .from_local_datetime(&(midnight + Duration::hours(1)))
                .earliest()
        })
        .map_or(0, |t| t.timestamp());
    t.max(0) as u64
}

/// The days a date expression covers, as `[start, end)`.
fn day_span(v: &str, today: NaiveDate) -> Option<(NaiveDate, NaiveDate)> {
    let month_start = |d: NaiveDate| d.with_day(1).unwrap();
    let next_month = |d: NaiveDate| {
        month_start(d)
            .checked_add_months(chrono::Months::new(1))
            .unwrap()
    };
    let year_start = |y: i32| NaiveDate::from_ymd_opt(y, 1, 1);
    let week_start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    let day = Duration::days(1);
    Some(match v {
        "today" => (today, today + day),
        "yesterday" => (today - day, today),
        "thisweek" => (week_start, week_start + Duration::days(7)),
        "lastweek" => (week_start - Duration::days(7), week_start),
        "thismonth" => (month_start(today), next_month(today)),
        "lastmonth" => {
            let start = month_start(today).checked_sub_months(chrono::Months::new(1))?;
            (start, month_start(today))
        }
        "thisyear" => (year_start(today.year())?, year_start(today.year() + 1)?),
        "lastyear" => (year_start(today.year() - 1)?, year_start(today.year())?),
        _ => {
            let parts: Vec<&str> = v.split(['-', '/', '.']).collect();
            let num = |i: usize| parts.get(i).and_then(|p| p.parse::<u32>().ok());
            let year = parts
                .first()
                .filter(|p| p.len() == 4)
                .and_then(|p| p.parse::<i32>().ok())?;
            match parts.len() {
                1 => (year_start(year)?, year_start(year + 1)?),
                2 => {
                    let d = NaiveDate::from_ymd_opt(year, num(1)?, 1)?;
                    (d, next_month(d))
                }
                3 => {
                    let d = NaiveDate::from_ymd_opt(year, num(1)?, num(2)?)?;
                    (d, d + day)
                }
                _ => return None,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(v: &str) -> RangeInclusive<u64> {
        size_range(v).unwrap()
    }

    #[test]
    fn sizes() {
        assert_eq!(size(">10mb"), 10 * 1024 * 1024 + 1..=u64::MAX);
        assert_eq!(size(">=1k"), 1024..=u64::MAX);
        assert_eq!(size("<1.5k"), 0..=1535);
        assert_eq!(size("<=2"), 0..=2);
        assert_eq!(size("100"), 100..=100);
        assert_eq!(size("1m..2m"), 1 << 20..=2 << 20);
        assert_eq!(size("empty"), 0..=0);
        assert!(size_range("<0").is_none());
        assert!(size_range("10q").is_none());
        assert!(size_range("2m..1m").is_none());
        assert!(size_range("").is_none());
    }

    #[test]
    fn dates() {
        let today = NaiveDate::from_ymd_opt(2026, 3, 4).unwrap(); // a Wednesday
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        let span = |v| day_span(v, today).unwrap();
        assert_eq!(span("today"), (d(2026, 3, 4), d(2026, 3, 5)));
        assert_eq!(span("thisweek"), (d(2026, 3, 2), d(2026, 3, 9)));
        assert_eq!(span("lastweek"), (d(2026, 2, 23), d(2026, 3, 2)));
        assert_eq!(span("lastmonth"), (d(2026, 2, 1), d(2026, 3, 1)));
        assert_eq!(span("2024"), (d(2024, 1, 1), d(2025, 1, 1)));
        assert_eq!(span("2024-12"), (d(2024, 12, 1), d(2025, 1, 1)));
        assert_eq!(span("2024/2/29"), (d(2024, 2, 29), d(2024, 3, 1)));
        assert!(day_span("2023-02-29", today).is_none());
        assert!(day_span("24-01", today).is_none());

        let r = date_range(">=2024-05-01", today).unwrap();
        assert_eq!(*r.start(), secs(d(2024, 5, 1)));
        let r = date_range(">2024-05", today).unwrap();
        assert_eq!(*r.start(), secs(d(2024, 6, 1)));
        let r = date_range("<2024", today).unwrap();
        assert_eq!(*r.end(), secs(d(2024, 1, 1)) - 1);
        let r = date_range("2024-01..2024-03", today).unwrap();
        assert_eq!(r, secs(d(2024, 1, 1))..=secs(d(2024, 4, 1)) - 1);
    }

    #[test]
    fn tokens() {
        let today = NaiveDate::from_ymd_opt(2026, 3, 4).unwrap();
        assert!(matches!(parse("readme", today), Token::Text));
        assert!(matches!(parse("c:", today), Token::Text));
        assert!(matches!(parse("size:", today), Token::Incomplete));
        assert!(matches!(parse("ext:", today), Token::Incomplete));
        assert!(matches!(parse("size:abc", today), Token::Error(_)));
        assert!(
            matches!(parse("ext:.txt;md", today), Token::Filter(Filter::Ext(e), None) if e == ["txt", "md"])
        );
        assert!(matches!(
            parse("folder:", today),
            Token::Filter(Filter::Dir(true), None)
        ));
        assert!(
            matches!(parse("file:abc", today), Token::Filter(Filter::Dir(false), Some(t)) if t == "abc")
        );
    }
}
