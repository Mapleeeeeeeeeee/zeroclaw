//! Taiwan public holiday / workday calendar, bundled at compile time.
//!
//! Data is sourced from the 行政院人事行政總處 annual calendar JSON
//! (asset file `assets/taiwan_calendar_2026.json`) and embedded via
//! `include_str!` so no file I/O occurs at runtime.
//!
//! Lookup is **TZ-agnostic**: this module accepts a bare [`NaiveDate`] and
//! returns whether that date is a workday in Taiwan. Callers are responsible
//! for converting "now" to Taiwan local time (Asia/Taipei, UTC+8) before
//! calling [`is_workday_tw`]. This keeps the calendar module free of any
//! timezone or async dependency.
//!
//! Only 2026 is bundled. For dates outside the covered range the function
//! returns [`None`]; callers decide the fallback policy (e.g. `water_reminder`
//! treats `None` as a workday).

use std::collections::HashMap;
use std::sync::OnceLock;

use chrono::{Datelike, NaiveDate};
use chrono_tz::Asia::Taipei;
use serde::Deserialize;

const JSON_2026: &str = include_str!("../../../assets/taiwan_calendar_2026.json");

#[derive(Deserialize)]
struct RuyutEntry {
    date: String,
    #[serde(rename = "isHoliday")]
    is_holiday: bool,
}

static CALENDAR: OnceLock<HashMap<NaiveDate, bool>> = OnceLock::new();

fn load_bundled() -> HashMap<NaiveDate, bool> {
    let entries: Vec<RuyutEntry> = serde_json::from_str(JSON_2026)
        .expect("bundled taiwan_calendar_2026.json must parse — this is a build-time invariant");

    let mut map = HashMap::with_capacity(entries.len());
    let mut min_year = i32::MAX;
    let mut max_year = i32::MIN;

    for entry in entries {
        let naive_date = NaiveDate::parse_from_str(&entry.date, "%Y%m%d").unwrap_or_else(|err| {
            panic!(
                "bundled taiwan_calendar_2026.json contains invalid date '{}': {err} — this is a build-time invariant",
                entry.date
            )
        });

        let year = naive_date.year();
        if year < min_year {
            min_year = year;
        }
        if year > max_year {
            max_year = year;
        }

        map.insert(naive_date, !entry.is_holiday);
    }

    tracing::info!(
        "[taiwan_calendar] loaded: {} entries, years {}-{}",
        map.len(),
        min_year,
        max_year
    );

    map
}

/// Returns today's date in Asia/Taipei timezone (UTC+8).
///
/// Converts the current UTC instant to Taiwan local time before extracting the
/// date, so callers on UTC servers get the correct Taiwan-local date.
pub(super) fn today_in_taipei() -> NaiveDate {
    chrono::Utc::now().with_timezone(&Taipei).date_naive()
}

/// Returns `Some(true)` if `date` is a workday in Taiwan, `Some(false)` if it
/// is a weekend or public holiday.  Returns `None` if the date is outside the
/// bundled calendar range.
///
/// Callers should compute the Taiwan-local date themselves before calling this
/// function (e.g. `Utc::now().with_timezone(&chrono_tz::Asia::Taipei).date_naive()`).
pub fn is_workday_tw(date: NaiveDate) -> Option<bool> {
    let calendar = CALENDAR.get_or_init(load_bundled);
    calendar.get(&date).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_workday_tw_holiday_2026_01_01() {
        // 開國紀念日 — should be non-workday
        let date = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        assert_eq!(is_workday_tw(date), Some(false));
    }

    #[test]
    fn is_workday_tw_regular_tuesday_2026_03_17() {
        // Regular Tuesday — should be a workday
        let date = NaiveDate::from_ymd_opt(2026, 3, 17).unwrap();
        assert_eq!(is_workday_tw(date), Some(true));
    }

    #[test]
    fn is_workday_tw_weekend_2026_03_14() {
        // Saturday — should be non-workday
        let date = NaiveDate::from_ymd_opt(2026, 3, 14).unwrap();
        assert_eq!(is_workday_tw(date), Some(false));
    }

    #[test]
    fn is_workday_tw_out_of_range_2099() {
        let date = NaiveDate::from_ymd_opt(2099, 1, 1).unwrap();
        assert_eq!(is_workday_tw(date), None);
    }

    #[test]
    fn bundle_loads_and_inverts_is_holiday_correctly() {
        // 2026-06-01 is a Monday with isHoliday=false in the bundled JSON.
        // Strong assertion catches:
        //   1. JSON parse regressions (would panic before returning)
        //   2. Boolean-inversion regressions (flipping `!entry.is_holiday` would make this Some(false))
        let result = is_workday_tw(NaiveDate::from_ymd_opt(2026, 6, 1).unwrap());
        assert_eq!(result, Some(true));
    }
}
