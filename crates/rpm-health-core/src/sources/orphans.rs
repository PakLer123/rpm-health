//! The report behind Fedora's weekly orphan announcements.
//!
//! This only tells us *when* a package was orphaned. Whether it is orphaned
//! comes from Koji, because the report also lists packages that have a real
//! owner in both Koji and Pagure. It's undocumented and hosted outside Fedora's
//! infrastructure, so callers must cope with it being unavailable.

use std::collections::HashMap;

use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};

use crate::error::{Error, Result};
use crate::http::Transport;

pub const REPORT_URL: &str = "https://a.gtmx.me/orphans/orphans.json";

/// Serde fills this in from JSON, ignoring the fields we don't list.
#[derive(Deserialize)]
struct Report {
    status_change: HashMap<String, String>,
}

/// Returns a map of source package name to the date it was orphaned.
pub fn fetch_orphan_dates(transport: &dyn Transport) -> Result<HashMap<String, Date>> {
    let body = transport.get(REPORT_URL)?.into_success_body(REPORT_URL)?;
    parse_report(&body)
}

fn parse_report(body: &str) -> Result<HashMap<String, Date>> {
    let report: Report = serde_json::from_str(body).map_err(|e| Error::Parse {
        what: "orphan report",
        message: e.to_string(),
    })?;
    // An unreadable date only costs us that date, so skip it. `filter_map` keeps
    // the `Some` results and drops the `None` ones.
    Ok(report
        .status_change
        .into_iter()
        .filter_map(|(name, timestamp)| {
            let parsed = OffsetDateTime::parse(&timestamp, &Rfc3339).ok()?;
            Some((name, parsed.date()))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::FakeTransport;

    const FIXTURE: &str = include_str!("../../tests/fixtures/orphans.json");

    #[test]
    fn keeps_only_the_date_from_each_timestamp() {
        let transport = FakeTransport::default().with_get(REPORT_URL, FIXTURE);
        let dates = fetch_orphan_dates(&transport).unwrap();
        assert_eq!(dates["clang15"].to_string(), "2026-10-26");
    }

    #[test]
    fn an_unreadable_date_only_loses_that_entry() {
        let body =
            "{\"status_change\": {\"bad\": \"soon\", \"good\": \"2026-10-26T21:47:09+00:00\"}}";
        let dates = parse_report(body).unwrap();
        assert_eq!(dates.len(), 1);
        assert!(dates.contains_key("good"));
    }

    #[test]
    fn a_missing_report_is_an_error() {
        let transport = FakeTransport::default();
        assert!(matches!(
            fetch_orphan_dates(&transport),
            Err(Error::BadStatus { status: 404, .. })
        ));
    }

    #[test]
    fn a_changed_format_is_an_error() {
        let transport = FakeTransport::default().with_get(REPORT_URL, "{\"something\": 1}");
        assert!(matches!(
            fetch_orphan_dates(&transport),
            Err(Error::Parse { .. })
        ));
    }
}
