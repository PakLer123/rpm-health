use time::{Date, OffsetDateTime};

pub fn today() -> Date {
    OffsetDateTime::now_utc().date()
}

/// Converts seconds since 1970 to a date, or `None` if it's out of range.
pub fn date_from_unix(seconds: i64) -> Option<Date> {
    OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .map(OffsetDateTime::date)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_a_real_changelog_time() {
        assert_eq!(
            date_from_unix(1784116800).unwrap().to_string(),
            "2026-07-15"
        );
    }

    #[test]
    fn out_of_range_values_are_none() {
        assert_eq!(date_from_unix(i64::MAX), None);
    }
}
