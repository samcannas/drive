//! The overnight upload window ("upload large files overnight").

use chrono::{DateTime, Duration, Local, NaiveTime, TimeZone, Timelike};

/// Parses "HH:MM" (24 h) into minutes after midnight.
pub fn parse_hhmm(s: &str) -> Option<u16> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

pub fn format_hhmm(min: u16) -> String {
    format!("{:02}:{:02}", min / 60, min % 60)
}

/// A daily window in local time; `end` before `start` wraps past midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NightWindow {
    pub start: u16,
    pub end: u16,
}

impl NightWindow {
    pub fn parse(start: &str, end: &str) -> Option<Self> {
        Some(Self {
            start: parse_hhmm(start)?,
            end: parse_hhmm(end)?,
        })
    }

    fn minute_of(t: &DateTime<Local>) -> u16 {
        (t.hour() * 60 + t.minute()) as u16
    }

    pub fn is_open_at(&self, t: &DateTime<Local>) -> bool {
        let m = Self::minute_of(t);
        if self.start == self.end {
            return true; // a full day
        }
        if self.start < self.end {
            m >= self.start && m < self.end
        } else {
            m >= self.start || m < self.end
        }
    }

    pub fn is_open(&self) -> bool {
        self.is_open_at(&Local::now())
    }

    /// When the window next opens (now if it is open).
    pub fn next_open_at(&self, t: &DateTime<Local>) -> DateTime<Local> {
        if self.is_open_at(t) {
            return *t;
        }
        let start = NaiveTime::from_hms_opt((self.start / 60) as u32, (self.start % 60) as u32, 0)
            .expect("valid time");
        let today = t.date_naive().and_time(start);
        let candidate = if Self::minute_of(t) < self.start {
            today
        } else {
            today + Duration::days(1)
        };
        // DST gaps: fall back to the earliest valid instant.
        Local
            .from_local_datetime(&candidate)
            .earliest()
            .unwrap_or_else(|| *t + Duration::hours(1))
    }

    pub fn start_label(&self) -> String {
        format_hhmm(self.start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 11, 11, h, m, 0)
            .earliest()
            .unwrap()
    }

    #[test]
    fn parse_and_format() {
        assert_eq!(parse_hhmm("01:00"), Some(60));
        assert_eq!(parse_hhmm(" 7:05"), Some(425));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("1h"), None);
        assert_eq!(format_hhmm(425), "07:05");
    }

    #[test]
    fn windows() {
        let night = NightWindow::parse("23:00", "06:00").unwrap();
        assert!(night.is_open_at(&at(23, 30)));
        assert!(night.is_open_at(&at(2, 0)));
        assert!(!night.is_open_at(&at(6, 0)));
        assert!(!night.is_open_at(&at(12, 0)));
        assert_eq!(night.next_open_at(&at(12, 0)), at(23, 0));
        let early = NightWindow::parse("01:00", "07:00").unwrap();
        assert!(!early.is_open_at(&at(0, 30)));
        assert_eq!(early.next_open_at(&at(0, 30)), at(1, 0));
        assert_eq!(early.next_open_at(&at(8, 0)), at(1, 0) + Duration::days(1));
        assert_eq!(early.next_open_at(&at(3, 0)), at(3, 0));
    }
}
