use chrono::{DateTime, Days, LocalResult, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

pub fn first_anchor(now: i64, timezone: &str, start_time: &str) -> Result<i64, String> {
    let zone: Tz = timezone.parse().map_err(|_| "Invalid timezone")?;
    if start_time.len() != 5 {
        return Err("start_time must be HH:MM".into());
    }
    let time =
        NaiveTime::parse_from_str(start_time, "%H:%M").map_err(|_| "start_time must be HH:MM")?;
    let instant: DateTime<Utc> = DateTime::from_timestamp(now, 0).ok_or("Time out of range")?;
    let mut date = instant.with_timezone(&zone).date_naive();
    loop {
        let occurrence = match zone.from_local_datetime(&date.and_time(time)) {
            LocalResult::Single(value) => Some(value.timestamp()).filter(|v| *v >= now),
            LocalResult::Ambiguous(a, b) => [a.timestamp(), b.timestamp()]
                .into_iter()
                .filter(|v| *v >= now)
                .min(),
            LocalResult::None => None,
        };
        if let Some(value) = occurrence {
            return Ok(value);
        }
        date = date.checked_add_days(Days::new(1)).ok_or("Time overflow")?;
    }
}

pub fn next_after(anchor: i64, interval_seconds: u64, now: i64) -> Result<i64, String> {
    if interval_seconds == 0 {
        return Err("Interval must be positive".into());
    }
    if anchor > now {
        return Ok(anchor);
    }
    let interval = i128::from(interval_seconds);
    let steps = (i128::from(now) - i128::from(anchor)) / interval + 1;
    i64::try_from(i128::from(anchor) + steps * interval).map_err(|_| "Time overflow".into())
}
