//! Unix time to Copenhagen's wall-clock time: CET (UTC+1), or CEST (UTC+2)
//! from 01:00 UTC on the last Sunday of March to 01:00 UTC on the last Sunday
//! of October.

const DAY: i64 = 86_400;
const HOUR: i64 = 3_600;

/// The hour and minute in Copenhagen at `unix` seconds since 1970 (UTC).
pub fn copenhagen(unix: i64) -> (u8, u8) {
    let offset = if in_summer_time(unix) { 2 } else { 1 } * HOUR;
    let seconds_of_day = (unix + offset).rem_euclid(DAY);
    ((seconds_of_day / HOUR) as u8, (seconds_of_day % HOUR / 60) as u8)
}

fn in_summer_time(unix: i64) -> bool {
    let year = year_of(unix.div_euclid(DAY));
    let start = last_sunday(year, 3) * DAY + HOUR;
    let end = last_sunday(year, 10) * DAY + HOUR;
    (start..end).contains(&unix)
}

/// The day number (since 1970-01-01) of the last Sunday of a 31-day `month`.
fn last_sunday(year: i64, month: i64) -> i64 {
    let last = days_from_civil(year, month, 31);
    // 1970-01-01 was a Thursday; 0 is Sunday.
    let weekday = (last + 4).rem_euclid(7);
    last - weekday
}

/// Days since 1970-01-01 of a date in the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The year of a day number (since 1970-01-01).
fn year_of(days: i64) -> i64 {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let year = year_of_era + era * 400;
    if month_from_march >= 10 { year + 1 } else { year }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winter_and_summer() {
        // 2026-01-15 12:00:00 UTC is 13:00 in Copenhagen.
        assert_eq!(copenhagen(days_from_civil(2026, 1, 15) * DAY + 12 * HOUR), (13, 0));
        // 2026-07-01 12:30:00 UTC is 14:30.
        assert_eq!(copenhagen(days_from_civil(2026, 7, 1) * DAY + 12 * HOUR + 30 * 60), (14, 30));
    }

    #[test]
    fn changeovers() {
        // 2026: summer time starts Sunday 29 March and ends Sunday 25 October.
        assert_eq!(last_sunday(2026, 3), days_from_civil(2026, 3, 29));
        assert_eq!(last_sunday(2026, 10), days_from_civil(2026, 10, 25));
        let start = days_from_civil(2026, 3, 29) * DAY + HOUR;
        assert_eq!(copenhagen(start - 1), (1, 59));
        assert_eq!(copenhagen(start), (3, 0));
        let end = days_from_civil(2026, 10, 25) * DAY + HOUR;
        assert_eq!(copenhagen(end - 1), (2, 59));
        assert_eq!(copenhagen(end), (2, 0));
    }

    #[test]
    fn years() {
        assert_eq!(year_of(0), 1970);
        assert_eq!(year_of(days_from_civil(2026, 12, 31)), 2026);
        assert_eq!(year_of(days_from_civil(2027, 1, 1)), 2027);
        assert_eq!(year_of(days_from_civil(2028, 2, 29)), 2028);
    }
}
