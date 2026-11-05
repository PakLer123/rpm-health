//! The report behind Fedora's weekly orphan announcements.
//!
//! This only tells us *when* a package was orphaned. Whether it is orphaned
//! comes from Koji, because the report also lists packages that have a real
//! owner in both Koji and Pagure. It's undocumented and hosted outside Fedora's
//! infrastructure, so callers must cope with it being unavailable.

use std::collections::HashMap;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::http::Transport;

pub const REPORT_URL: &str = "https://a.gtmx.me/orphans/orphans.json";

/// Serde fills this in from JSON, ignoring the fields we don't list.
#[derive(Deserialize)]
struct Report {
    status_change: HashMap<String, String>,
}

/// Returns a map of source package name to `YYYY-MM-DD` date.
pub fn fetch_orphan_dates(transport: &dyn Transport) -> Result<HashMap<String, String>> {
    let body = transport.get(REPORT_URL)?.into_success_body(REPORT_URL)?;
    parse_report(&body)
}

fn parse_report(body: &str) -> Result<HashMap<String, String>> {
    let report: Report = serde_json::from_str(body).map_err(|e| Error::Parse {
        what: "orphan report",
        message: e.to_string(),
    })?;
    Ok(report
        .status_change
        .into_iter()
        .map(|(name, timestamp)| {
            let date = timestamp.split('T').next().unwrap_or_default().to_string();
            (name, date)
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
        assert_eq!(dates["clang15"], "2026-10-26");
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
