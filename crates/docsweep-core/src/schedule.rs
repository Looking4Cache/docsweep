//! Cron expressions of `portal.schedule`: five fields, evaluated in a time zone
//! the caller chooses, which is local time everywhere but in tests.

use chrono::{DateTime, TimeZone};
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};

/// The number of fields in a cron expression.
const FIELDS: usize = 5;

/// What a schedule must look like, for error messages.
const EXPECTED: &str = "must be a cron expression with five fields (minute hour day month weekday), for example `0 9 10 * *`";

/// A valid cron expression that also comes true at some point.
#[derive(Debug, Clone)]
pub struct Schedule(Cron);

impl Schedule {
    /// Parses an expression. The message says what is wrong in plain words.
    pub fn parse(expression: &str) -> Result<Self, String> {
        if expression.split_whitespace().count() != FIELDS {
            return Err(EXPECTED.to_owned());
        }
        let parser = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .year(Year::Disallowed)
            .build();
        let cron = parser
            .parse(expression)
            .map_err(|error| format!("{EXPECTED}: {error}"))?;
        let schedule = Self(cron);
        let probe = chrono::Utc
            .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
            .single()
            .expect("a fixed, valid date");
        if schedule.next_after(&probe).is_none() {
            return Err(format!(
                "{EXPECTED}: this expression never comes true, for example because that date does not exist"
            ));
        }
        Ok(schedule)
    }

    /// The first time after `after` at which this schedule is due.
    pub fn next_after<Tz: TimeZone>(&self, after: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        self.0.find_next_occurrence(after, false).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn at(offset_hours: i32, y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(offset_hours * 3600)
            .unwrap()
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
    }

    #[test]
    fn the_next_run_is_in_the_given_time_zone() {
        let schedule = Schedule::parse("0 9 10 * *").unwrap();
        let next = schedule.next_after(&at(1, 2026, 11, 6, 12, 0)).unwrap();
        assert_eq!(next, at(1, 2026, 11, 10, 9, 0));
        assert_eq!(next.to_utc(), at(0, 2026, 11, 10, 8, 0).to_utc());
    }

    #[test]
    fn a_due_moment_itself_is_not_next() {
        let schedule = Schedule::parse("0 9 10 * *").unwrap();
        let next = schedule.next_after(&at(0, 2026, 11, 10, 9, 0)).unwrap();
        assert_eq!(next, at(0, 2026, 12, 10, 9, 0));
    }

    #[test]
    fn common_forms_are_accepted() {
        for expression in [
            "0 9 10 * *",
            "*/15 * * * *",
            "0 8 * * 1-5",
            "30 6 1,15 * *",
            "0 7 * * MON",
            " 0  9 10 * * ",
        ] {
            assert!(Schedule::parse(expression).is_ok(), "{expression}");
        }
    }

    #[test]
    fn invalid_expressions_are_rejected_with_a_clear_message() {
        for expression in [
            "",
            "nope",
            "@daily",
            "0 9 10 *",
            "0 0 9 10 * *",
            "0 9 10 * * 2030",
            "60 9 10 * *",
            "0 24 10 * *",
            "0 9 32 * *",
            "0 9 10 13 *",
            "a b c d e",
        ] {
            let message = Schedule::parse(expression).unwrap_err();
            assert!(message.contains("five fields"), "{expression}: {message}");
            assert!(message.contains("0 9 10 * *"), "{expression}: {message}");
        }
    }

    #[test]
    fn an_expression_that_never_comes_true_is_rejected() {
        let message = Schedule::parse("0 9 31 2 *").unwrap_err();
        assert!(message.contains("never comes true"), "{message}");
    }
}
