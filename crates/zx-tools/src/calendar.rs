//! Dates of the Gregorian calendar, which the standard library does not have, for the times
//! that file names and prompts show.

/// The date `days` after 1970-01-01, as year, month (1 to 12) and day (1 to 31). Counted in
/// 400-year cycles of 146097 days that start on March 1, so that a leap day ends its year
/// (Howard Hinnant, "chrono-Compatible Low-Level Date Algorithms", `civil_from_days`).
pub(crate) fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let (cycle, day_of_cycle) = (shifted / 146_097, shifted % 146_097);
    let year_of_cycle = (day_of_cycle - day_of_cycle / 1460 + day_of_cycle / 36_524 - day_of_cycle / 146_096) / 365;
    let day_of_year = day_of_cycle - (365 * year_of_cycle + year_of_cycle / 4 - year_of_cycle / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_cycle + cycle * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_since_the_epoch_are_dates_with_leap_days_where_they_fall() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(19_675), (2023, 11, 14));
    }
}
