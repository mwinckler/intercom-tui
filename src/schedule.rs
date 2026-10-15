//! The plugin's English date grammar, with explicit locale and DST handling.
use anyhow::{Result, bail};
use chrono::{
    DateTime, Datelike, Duration, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Timelike,
};
use regex::Regex;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static schedule regex")
}

pub fn system_month_first() -> bool {
    let locale = ["LC_ALL", "LC_TIME", "LANG"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|s| !s.is_empty()))
        .unwrap_or_default();
    ["en_US", "en_CA", "en_PH"]
        .iter()
        .any(|prefix| locale.starts_with(prefix))
}

pub fn label(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|date| date.format("%a %b %-d, %Y at %H:%M %Z").to_string())
        .unwrap_or_else(|| "Invalid date".into())
}

pub fn local_date(date: NaiveDate, hour: u32, minute: u32, now: i64) -> Result<i64> {
    let naive = date
        .and_hms_opt(hour, minute, 0)
        .ok_or_else(|| anyhow::anyhow!("Choose a valid time"))?;
    let at = match Local.from_local_datetime(&naive) {
        LocalResult::Single(at) => at.timestamp(),
        LocalResult::Ambiguous(_, _) => {
            bail!("That time occurs twice during a daylight-saving change. Choose another time.")
        }
        LocalResult::None => {
            bail!("That time does not exist during a daylight-saving change. Choose another time.")
        }
    };
    if at <= now {
        bail!("That time is in the past");
    }
    Ok(at)
}

pub fn parse(text: &str, now: DateTime<Local>, month_first: bool) -> Result<i64> {
    let mut rest = text.trim().to_lowercase().replace(',', " ");
    rest = rest.split_whitespace().collect::<Vec<_>>().join(" ");
    if rest.is_empty() {
        bail!("Enter a date or time");
    }
    if re(r"\byesterday\b|\blast\s+").is_match(&rest) {
        bail!("That time is in the past");
    }
    if let Some(caps) =
        re(r"^in\s+(\d+)\s*(minutes?|mins?|m|hours?|hrs?|h|days?|d|weeks?|w)$").captures(&rest)
    {
        let n: i64 = caps[1].parse()?;
        let seconds = match caps[2].chars().next().unwrap() {
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            _ => 604800,
        };
        if n <= 0 {
            bail!("That time is in the past");
        }
        return now
            .timestamp()
            .checked_add(
                n.checked_mul(seconds)
                    .ok_or_else(|| anyhow::anyhow!("Date too far in the future"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("Date too far in the future"));
    }
    let this_period = rest.starts_with("this ");
    let next_weekday = rest.starts_with("next ");
    if this_period || next_weekday {
        rest = rest.split_once(' ').unwrap().1.into();
    }
    let mut day_offset = None;
    let mut tonight = false;
    for (word, offset) in [("tomorrow", 1), ("today", 0), ("tonight", 0)] {
        let pattern = re(&format!(r"\b{word}\b"));
        if pattern.is_match(&rest) {
            if word == "tonight" {
                tonight = true;
            } else {
                day_offset = Some(offset);
            }
            rest = pattern.replace(&rest, " ").into_owned();
        }
    }
    let weekday_names = [
        "sunday",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
    ];
    let weekday_pattern = re(
        r"\b(sunday|sun|monday|mon|tuesday|tue|tues|wednesday|wed|weds|thursday|thu|thur|thurs|friday|fri|saturday|sat)\b",
    );
    let weekday = weekday_pattern.captures(&rest).map(|caps| {
        weekday_names
            .iter()
            .position(|day| day.starts_with(&caps[1][..3]))
            .unwrap() as i64
    });
    rest = weekday_pattern.replace(&rest, " ").into_owned();
    let mut calendar: Option<(Option<i32>, u32, u32)> = None;
    let iso = re(r"\b(\d{4})-(\d{1,2})-(\d{1,2})\b");
    if let Some(caps) = iso.captures(&rest) {
        calendar = Some((Some(caps[1].parse()?), caps[2].parse()?, caps[3].parse()?));
        rest = iso.replace(&rest, " ").into_owned();
    } else {
        let months = [
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
        let names = "january|jan|february|feb|march|mar|april|apr|may|june|jun|july|jul|august|aug|september|sep|sept|october|oct|november|nov|december|dec";
        let month_first_pattern = re(&format!(
            r"\b({names})\s+(\d{{1,2}})(?:st|nd|rd|th)?(?:\s+(\d{{2,4}}))?\b"
        ));
        let day_first_pattern = re(&format!(
            r"\b(\d{{1,2}})(?:st|nd|rd|th)?\s+({names})(?:\s+(\d{{2,4}}))?\b"
        ));
        for (pattern, month_index, day_index) in
            [(&month_first_pattern, 1, 2), (&day_first_pattern, 2, 1)]
        {
            if let Some(caps) = pattern.captures(&rest) {
                let month = months
                    .iter()
                    .position(|m| m.starts_with(&caps[month_index]))
                    .ok_or_else(|| anyhow::anyhow!("Invalid month"))?
                    as u32
                    + 1;
                let year = caps.get(3).map(|v| expand_year(v.as_str())).transpose()?;
                calendar = Some((year, month, caps[day_index].parse()?));
                rest = pattern.replace(&rest, " ").into_owned();
                break;
            }
        }
        if calendar.is_none() {
            let numeric = re(r"\b(\d{1,2})[/-](\d{1,2})(?:[/-](\d{2,4}))?\b");
            if let Some(caps) = numeric.captures(&rest) {
                let first: u32 = caps[1].parse()?;
                let second: u32 = caps[2].parse()?;
                let (month, day) = if first > 12 || (second <= 12 && !month_first) {
                    (second, first)
                } else {
                    (first, second)
                };
                calendar = Some((
                    caps.get(3).map(|v| expand_year(v.as_str())).transpose()?,
                    month,
                    day,
                ));
                rest = numeric.replace(&rest, " ").into_owned();
            }
        }
    }
    let period_pattern = re(r"\b(morning|afternoon|evening|night|noon|midnight)\b");
    let period = period_pattern
        .captures(&rest)
        .map(|c| c[1].to_owned())
        .or_else(|| tonight.then(|| "night".into()));
    rest = period_pattern.replace(&rest, " ").into_owned();
    let mut hour = match period.as_deref() {
        Some("afternoon") => 13,
        Some("evening") => 18,
        Some("night") => 21,
        Some("noon") => 12,
        Some("midnight") => 0,
        _ => 9,
    };
    let mut minute = 0;
    let mut had_time = false;
    for pattern in [
        r"\b(\d{1,2}):(\d{2})\s*(a\.?m\.?|p\.?m\.?)?\b",
        r"\b(\d{1,2})\s*(a\.?m\.?|p\.?m\.?)\b",
        r"\bat\s+(\d{1,2})\b",
    ] {
        let regex = re(pattern);
        if let Some(caps) = regex.captures(&rest) {
            hour = caps[1].parse()?;
            let colon = pattern.contains(":(");
            let meridian = if colon {
                minute = caps[2].parse()?;
                caps.get(3).map(|v| v.as_str())
            } else {
                caps.get(2).map(|v| v.as_str())
            };
            if let Some(mark) = meridian {
                if !(1..=12).contains(&hour) {
                    bail!("Choose a valid hour");
                }
                hour %= 12;
                if mark.starts_with('p') {
                    hour += 12;
                }
            }
            had_time = true;
            rest = regex.replace(&rest, " ").into_owned();
            break;
        }
    }
    rest = re(r"\b(at|on|the|around)\b")
        .replace_all(&rest, " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !had_time && re(r"^\d{1,2}$").is_match(&rest) {
        hour = rest.parse()?;
        rest.clear();
        had_time = true;
    }
    if !rest.is_empty() {
        bail!("Could not understand that date");
    }
    if hour > 23 || minute > 59 {
        bail!("Choose a valid time");
    }
    if (day_offset.is_some() || tonight) && (calendar.is_some() || weekday.is_some()) {
        bail!("Choose one date, not multiple dates");
    }
    let today = now.date_naive();
    let date = if let Some((year, month, day)) = calendar {
        if let Some(year) = year {
            NaiveDate::from_ymd_opt(year, month, day)
                .ok_or_else(|| anyhow::anyhow!("Choose a valid date"))?
        } else {
            let mut result = None;
            for year in now.year()..now.year() + 9 {
                if let Some(date) = NaiveDate::from_ymd_opt(year, month, day)
                    && date
                        .and_hms_opt(hour, minute, 0)
                        .is_some_and(|dt| dt > now.naive_local())
                {
                    result = Some(date);
                    break;
                }
            }
            result.ok_or_else(|| anyhow::anyhow!("Choose a valid date"))?
        }
    } else if let Some(weekday) = weekday {
        let mut delta = (weekday - now.weekday().num_days_from_sunday() as i64 + 7) % 7;
        if delta == 0 && (next_weekday || (hour, minute) <= (now.hour(), now.minute())) {
            delta = 7;
        }
        today
            .checked_add_signed(Duration::days(delta))
            .ok_or_else(|| anyhow::anyhow!("Invalid date"))?
    } else {
        today + Duration::days(day_offset.unwrap_or(0))
    };
    let naive: NaiveDateTime = date
        .and_hms_opt(hour, minute, 0)
        .ok_or_else(|| anyhow::anyhow!("Choose a valid time"))?;
    if naive <= now.naive_local() {
        if this_period || (tonight && !had_time) {
            return Ok(now.timestamp() + 3600);
        }
        if calendar.is_none() && weekday.is_none() && day_offset.is_none() && !tonight {
            return local_date(date + Duration::days(1), hour, minute, now.timestamp());
        }
    }
    local_date(date, hour, minute, now.timestamp())
}

fn expand_year(text: &str) -> Result<i32> {
    let year: i32 = text.parse()?;
    Ok(if year < 70 {
        year + 2000
    } else if year < 100 {
        year + 1900
    } else {
        year
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plugin_schedule_examples() {
        let now = Local.with_ymd_and_hms(2026, 8, 21, 14, 30, 0).unwrap();
        for (text, year, month, day, hour, minute) in [
            ("22:10", 2026, 8, 21, 22, 10),
            ("tomorrow at 8", 2026, 8, 22, 8, 0),
            ("Monday", 2026, 8, 24, 9, 0),
            ("tonight", 2026, 8, 21, 21, 0),
            ("April 7", 2027, 4, 7, 9, 0),
            ("this morning", 2026, 8, 21, 15, 30),
            ("Tuesday afternoon", 2026, 8, 25, 13, 0),
            ("8am", 2026, 8, 22, 8, 0),
            ("2026-12-31 22:10", 2026, 12, 31, 22, 10),
            ("7 April 2027 at noon", 2027, 4, 7, 12, 0),
        ] {
            assert_eq!(
                parse(text, now, false).unwrap(),
                Local
                    .with_ymd_and_hms(year, month, day, hour, minute, 0)
                    .unwrap()
                    .timestamp(),
                "{text}"
            );
        }
        assert_eq!(parse("in 2h", now, false).unwrap(), now.timestamp() + 7200);
        for text in [
            "yesterday",
            "today at 8",
            "in 0 minutes",
            "31/31",
            "2026-02-30",
            "25:00",
            "nonsense",
            "tomorrow Monday",
        ] {
            assert!(parse(text, now, false).is_err(), "{text}");
        }
    }
    #[test]
    fn numeric_dates_follow_locale() {
        let now = Local.with_ymd_and_hms(2026, 3, 1, 10, 0, 0).unwrap();
        let us = Local
            .timestamp_opt(parse("4/7", now, true).unwrap(), 0)
            .unwrap();
        let gb = Local
            .timestamp_opt(parse("4/7", now, false).unwrap(), 0)
            .unwrap();
        assert_eq!((us.month(), us.day()), (4, 7));
        assert_eq!((gb.month(), gb.day()), (7, 4));
    }

    #[test]
    fn daylight_saving_gaps_and_ambiguous_times_require_another_time() {
        let before = Local
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        for (month, day, hour) in [(3, 8, 2), (11, 1, 1)] {
            let date = NaiveDate::from_ymd_opt(2026, month, day).unwrap();
            let result = local_date(date, hour, 30, before);
            match Local.from_local_datetime(&date.and_hms_opt(hour, 30, 0).unwrap()) {
                LocalResult::Single(at) => assert_eq!(result.unwrap(), at.timestamp()),
                LocalResult::None | LocalResult::Ambiguous(_, _) => {
                    assert!(result.unwrap_err().to_string().contains("daylight-saving"))
                }
            }
        }
    }
}
