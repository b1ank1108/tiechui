//! Five-field cron in local time, `/loop` intervals and deterministic jitter.

pub const MINUTE: i64 = 60_000;
const DAY: i64 = 1440 * MINUTE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Local {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub offset_minutes: i64,
}

/// Local calendar conversion. `resolve` returns `None` for wall times a DST change skips
/// and one fixed instant for wall times it repeats.
pub trait Zone {
    fn local(&self, ms: i64) -> Local;
    fn resolve(&self, year: i64, month: u32, day: u32, hour: u32, minute: u32) -> Option<i64>;
}

/// The process's local time zone.
pub struct System;

#[cfg(unix)]
impl Zone for System {
    fn local(&self, ms: i64) -> Local {
        let seconds = ms.div_euclid(1000) as libc::time_t;
        // SAFETY: `tm` is plain data that `localtime_r` fills; both pointers outlive the call.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
            return utc(ms);
        }
        Local {
            year: i64::from(tm.tm_year) + 1900,
            month: tm.tm_mon as u32 + 1,
            day: tm.tm_mday as u32,
            hour: tm.tm_hour as u32,
            minute: tm.tm_min as u32,
            offset_minutes: tm.tm_gmtoff as i64 / 60,
        }
    }

    fn resolve(&self, year: i64, month: u32, day: u32, hour: u32, minute: u32) -> Option<i64> {
        // SAFETY: `mktime` reads and normalizes the zeroed, locally owned `tm`.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = i32::try_from(year - 1900).ok()?;
        tm.tm_mon = month as i32 - 1;
        tm.tm_mday = day as i32;
        tm.tm_hour = hour as i32;
        tm.tm_min = minute as i32;
        tm.tm_isdst = -1;
        let ms = unsafe { libc::mktime(&mut tm) } as i64 * 1000;
        let back = self.local(ms);
        ((back.year, back.month, back.day, back.hour, back.minute)
            == (year, month, day, hour, minute))
            .then_some(ms)
    }
}

#[cfg(not(unix))]
impl Zone for System {
    fn local(&self, ms: i64) -> Local {
        utc(ms)
    }

    fn resolve(&self, year: i64, month: u32, day: u32, hour: u32, minute: u32) -> Option<i64> {
        Some(days_from_civil(year, month, day) * DAY + i64::from(hour * 60 + minute) * MINUTE)
    }
}

pub fn utc(ms: i64) -> Local {
    let (year, month, day) = civil_from_days(ms.div_euclid(DAY));
    let minutes = ms.rem_euclid(DAY) / MINUTE;
    Local {
        year,
        month,
        day,
        hour: (minutes / 60) as u32,
        minute: (minutes % 60) as u32,
        offset_minutes: 0,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minute: u64,
    hour: u64,
    day: u64,
    month: u64,
    weekday: u64,
}

impl Cron {
    pub fn parse(text: &str) -> Result<Self, String> {
        let invalid = || {
            format!(
                "Invalid cron expression '{text}'. Expected 5 fields: minute hour day-of-month month day-of-week."
            )
        };
        let fields: Vec<&str> = text.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(invalid());
        };
        Ok(Self {
            minute: field(minute, 0, 59, false).ok_or_else(invalid)?,
            hour: field(hour, 0, 23, false).ok_or_else(invalid)?,
            day: field(day, 1, 31, false).ok_or_else(invalid)?,
            month: field(month, 1, 12, false).ok_or_else(invalid)?,
            weekday: field(weekday, 0, 6, true).ok_or_else(invalid)?,
        })
    }

    /// The first matching minute strictly after `after`, within the next 366 days.
    pub fn next(&self, after: i64, zone: &dyn Zone) -> Option<i64> {
        let start = zone.local(after);
        let first = days_from_civil(start.year, start.month, start.day);
        for days in first..=first + 366 {
            let (year, month, day) = civil_from_days(days);
            if !self.matches_day(month, day, (days + 4).rem_euclid(7) as u32) {
                continue;
            }
            for hour in bits(self.hour) {
                for minute in bits(self.minute) {
                    if days == first && (hour, minute) < (start.hour, start.minute) {
                        continue;
                    }
                    if let Some(ms) = zone.resolve(year, month, day, hour, minute)
                        && ms > after
                    {
                        return Some(ms);
                    }
                }
            }
        }
        None
    }

    fn matches_day(&self, month: u32, day: u32, weekday: u32) -> bool {
        let all_days = self.day == bits_between(1, 31);
        let all_weekdays = self.weekday == bits_between(0, 6);
        let by_day = self.day & 1 << day != 0;
        let by_weekday = self.weekday & 1 << weekday != 0;
        self.month & 1 << month != 0
            && match (all_days, all_weekdays) {
                (true, true) => true,
                (true, false) => by_weekday,
                (false, true) => by_day,
                (false, false) => by_day || by_weekday,
            }
    }
}

fn field(text: &str, min: u32, max: u32, weekday: bool) -> Option<u64> {
    let number = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| s.parse::<u32>().ok())
            .flatten()
    };
    let mut set = 0;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, number(step).filter(|&s| s > 0)?),
            None => (part, 1),
        };
        let (low, high) = match range.split_once('-') {
            _ if range == "*" => (min, max),
            Some((low, high)) => (number(low)?, number(high)?),
            None if step == 1 && !part.contains('/') => (number(range)?, number(range)?),
            None => return None,
        };
        // Day of week 7 is Sunday.
        let top = if weekday { 7 } else { max };
        if low < min || high > top || low > high {
            return None;
        }
        for value in (low..=high).step_by(step as usize) {
            set |= 1 << if weekday && value == 7 { 0 } else { value };
        }
    }
    Some(set)
}

fn bits(set: u64) -> impl Iterator<Item = u32> {
    (0..64).filter(move |bit| set & 1 << bit != 0)
}

fn bits_between(low: u32, high: u32) -> u64 {
    (low..=high).fold(0, |set, bit| set | 1 << bit)
}

/// The ID's leading 8 hex digits as a fraction in [0, 1); other IDs get no jitter.
fn fraction(id: &str) -> f64 {
    id.get(..8)
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .map_or(0.0, |value| f64::from(value) / 4_294_967_296.0)
}

/// Up to 10% of the period late, at most 15 minutes, stable per task.
pub fn recurring_fire(cron: &Cron, anchor: i64, id: &str, zone: &dyn Zone) -> Option<i64> {
    let first = cron.next(anchor, zone)?;
    let Some(second) = cron.next(first, zone) else {
        return Some(first);
    };
    let delay = (fraction(id) * 0.1 * (second - first) as f64) as i64;
    Some(first + delay.min(15 * MINUTE))
}

/// Up to 90 seconds early on the hour or half hour, never before creation.
pub fn one_shot_fire(cron: &Cron, created: i64, id: &str, zone: &dyn Zone) -> Option<i64> {
    let at = cron.next(created, zone)?;
    if zone.local(at).minute % 30 != 0 {
        return Some(at);
    }
    Some((at - (fraction(id) * 90_000.0) as i64).max(created))
}

#[derive(Debug, PartialEq, Eq)]
pub struct Interval {
    pub cron: String,
    pub minutes: u64,
}

/// The nearest evenly repeating cron period; ties go to the longer one.
pub fn interval(amount: u64, unit: char) -> Option<Interval> {
    let minutes = seconds(amount, unit)?.div_ceil(60);
    if minutes == 0 || minutes > 31 * 1440 {
        return None;
    }
    let minutes = [1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30]
        .into_iter()
        .chain([1, 2, 3, 4, 6, 8, 12].map(|hours| hours * 60))
        .chain((1..=31).map(|days| days * 1440))
        .min_by_key(|&candidate: &u64| (candidate.abs_diff(minutes), u64::MAX - candidate))?;
    let cron = match minutes {
        1 => "* * * * *".into(),
        m if m < 60 => format!("*/{m} * * * *"),
        60 => "0 * * * *".into(),
        m if m < 1440 => format!("0 */{} * * *", m / 60),
        1440 => "0 0 * * *".into(),
        m => format!("0 0 */{} * *", m / 1440),
    };
    Some(Interval { cron, minutes })
}

pub fn seconds(amount: u64, unit: char) -> Option<u64> {
    amount.checked_mul(match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return None,
    })
}

pub fn period(minutes: u64) -> String {
    let (amount, unit) = match minutes {
        m if m % 1440 == 0 => (m / 1440, "day"),
        m if m % 60 == 0 => (m / 60, "hour"),
        m => (m, "minute"),
    };
    if amount == 1 {
        format!("every {unit}")
    } else {
        format!("every {amount} {unit}s")
    }
}

/// A short reading of common shapes; anything else is shown as the expression.
pub fn describe(text: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const DAYS: [&str; 8] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    let fields: Vec<&str> = text.split_whitespace().collect();
    let [minute, hour, day, month, weekday] = fields[..] else {
        return text.into();
    };
    let number = |s: &str| s.parse::<u32>().ok();
    let step = |s: &str| s.strip_prefix("*/").and_then(number);
    let at = |h: u32, m: u32| format!("{h:02}:{m:02}");
    match (
        number(minute),
        number(hour),
        [day, month, weekday] == ["*"; 3],
    ) {
        _ if fields == ["*"; 5] => "every minute".into(),
        (None, _, true) if hour == "*" && step(minute).is_some() => {
            period(step(minute).map_or(1, u64::from))
        }
        (Some(0), None, true) if hour == "*" => "every hour".into(),
        (Some(m), None, true) if hour == "*" => format!("every hour at :{m:02}"),
        (Some(m), None, true) if step(hour).is_some() => {
            let every = period(step(hour).map_or(1, |h| u64::from(h) * 60));
            if m == 0 {
                every
            } else {
                format!("{every} at :{m:02}")
            }
        }
        (Some(m), Some(h), true) => format!("every day at {}", at(h, m)),
        (Some(m), Some(h), false) => match (number(day), number(month), step(day)) {
            _ if [day, month, weekday] == ["*", "*", "1-5"] => {
                format!("weekdays at {}", at(h, m))
            }
            _ if [day, month] == ["*"; 2] && number(weekday).is_some_and(|d| d <= 7) => {
                format!(
                    "every {} at {}",
                    DAYS[number(weekday).unwrap_or(0) as usize],
                    at(h, m)
                )
            }
            (Some(d), Some(mo), _) if weekday == "*" && (1..=12).contains(&mo) => {
                format!("on {} {d} at {}", MONTHS[mo as usize - 1], at(h, m))
            }
            (Some(d), None, _) if [month, weekday] == ["*"; 2] => {
                format!("monthly on day {d} at {}", at(h, m))
            }
            (None, None, Some(n)) if [month, weekday] == ["*"; 2] => {
                format!("at {} every {n} days of the month", at(h, m))
            }
            _ => format!("cron {text}"),
        },
        _ => format!("cron {text}"),
    }
}

/// `YYYY-MM-DD HH:MM ±HH:MM` in the zone.
pub fn format(ms: i64, zone: &dyn Zone) -> String {
    let local = zone.local(ms);
    let offset = local.offset_minutes.abs();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} {}{:02}:{:02}",
        local.year,
        local.month,
        local.day,
        local.hour,
        local.minute,
        if local.offset_minutes < 0 { '-' } else { '+' },
        offset / 60,
        offset % 60
    )
}

pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted + 2) / 5 + 1) as u32;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    } as u32;
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

#[derive(Debug, PartialEq, Eq)]
pub struct Request<'a> {
    pub amount: u64,
    pub unit: char,
    pub prompt: &'a str,
}

/// `[N<s|m|h|d>] prompt`, `prompt every N<unit>` or `prompt every N <unit word>`;
/// otherwise every 10 minutes. Only the interval is removed from the prompt.
pub fn request(arguments: &str) -> Option<Request<'_>> {
    let text = arguments.trim();
    let tokens: Vec<(usize, &str)> = text
        .split_whitespace()
        .map(|token| (token.as_ptr() as usize - text.as_ptr() as usize, token))
        .collect();
    let compact = |token: &str| {
        let unit = token.chars().last()?;
        let digits = token.strip_suffix(unit)?;
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && "smhd".contains(unit))
            .then(|| digits.parse::<u64>().map(|amount| (amount, unit)))
    };
    let word = |token: &str| {
        let token = token.to_ascii_lowercase();
        let token = token.strip_suffix('s').unwrap_or(&token).to_owned();
        ["second", "minute", "hour", "day"]
            .into_iter()
            .find(|unit| *unit == token)
            .and_then(|unit| unit.chars().next())
    };
    let every = |at: usize| tokens[at].1.eq_ignore_ascii_case("every");
    let (interval, prompt) = match tokens[..] {
        [(_, first), ..] if compact(first).is_some() => (compact(first)?, &text[first.len()..]),
        [.., (at, _), (_, last)] if every(tokens.len() - 2) && compact(last).is_some() => {
            (compact(last)?, &text[..at])
        }
        [.., (at, _), (_, amount), (_, unit)]
            if every(tokens.len() - 3)
                && amount.bytes().all(|b| b.is_ascii_digit())
                && word(unit).is_some() =>
        {
            (Ok((amount.parse().ok()?, word(unit)?)), &text[..at])
        }
        _ => (Ok((10, 'm')), text),
    };
    let (amount, unit) = interval.ok()?;
    let prompt = prompt.trim();
    (!prompt.is_empty()).then_some(Request {
        amount,
        unit,
        prompt,
    })
}
