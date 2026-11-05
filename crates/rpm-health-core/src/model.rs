use time::{Date, Duration};

/// Fedora retires packages that stay orphaned for this long.
const ORPHAN_GRACE_PERIOD: Duration = Duration::weeks(6);

/// One installed source package, with the binary RPMs built from it.
///
/// Fedora tracks maintainers per source package, so `kernel-core` and
/// `kernel-modules` both end up here under the source package `kernel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackage {
    pub source_name: String,
    pub binaries: Vec<String>,
    /// Usually one entry, but `kernel` often has several versions installed.
    pub versions: Vec<String>,
    pub from_fedora: bool,
    /// Newest first. Only from Fedora's own RPMs, so a third-party RPM sharing
    /// this source name can't make the package look active.
    pub changelog: Vec<ChangelogEntry>,
}

/// One entry from an RPM's changelog.
///
/// `derive(Ord)` compares fields in the order they're declared, so sorting
/// these sorts by date first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChangelogEntry {
    pub date: Date,
    pub author: String,
    pub text: String,
}

/// The result of the maintainer signal for one package.
///
/// A Rust enum is a value that is exactly one of these variants, and each
/// variant can carry its own data. `match` must handle every variant, so
/// adding a new status later makes the compiler point at all the code that
/// needs updating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintainerStatus {
    Maintained,
    /// `since` comes from the orphan report, when it has this package.
    Orphaned {
        since: Option<Date>,
    },
    /// Retired in Rawhide, so it won't be in the next Fedora release.
    Retiring {
        reason: String,
    },
    /// Retired from the Fedora release running on this system.
    Retired {
        reason: String,
    },
    /// We couldn't reach a verdict, and `reason` says why.
    Unknown {
        reason: String,
    },
}

impl MaintainerStatus {
    /// A short name for tables and summaries.
    ///
    /// `&self` means this method borrows the status: it can read it, but the
    /// caller keeps ownership and can go on using it afterwards.
    pub fn label(&self) -> &'static str {
        match self {
            MaintainerStatus::Maintained => "Maintained",
            MaintainerStatus::Orphaned { .. } => "Orphaned",
            MaintainerStatus::Retiring { .. } => "Retiring",
            MaintainerStatus::Retired { .. } => "Retired",
            MaintainerStatus::Unknown { .. } => "Unknown",
        }
    }

    /// A human-readable explanation of the status.
    pub fn reason(&self) -> String {
        match self {
            MaintainerStatus::Maintained => "Has a maintainer".to_string(),
            MaintainerStatus::Orphaned { since: Some(date) } => {
                let retirement = *date + ORPHAN_GRACE_PERIOD;
                format!(
                    "No maintainer since {date}; retirement due around {retirement} unless adopted"
                )
            }
            MaintainerStatus::Orphaned { since: None } => {
                "No maintainer; retired after six weeks unless adopted".to_string()
            }
            MaintainerStatus::Retiring { reason }
            | MaintainerStatus::Retired { reason }
            | MaintainerStatus::Unknown { reason } => reason.clone(),
        }
    }
}

/// The result of the Fedora activity signal for one package.
///
/// Being quiet isn't a verdict. A finished, stable package can go years without
/// needing changes, so this is only one input to a package's overall health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityStatus {
    /// Changed recently by something other than a Fedora-wide mass rebuild.
    Active {
        last_change: Date,
    },
    /// Nothing recent, and we know when the last change was.
    Quiet {
        last_change: Date,
    },
    /// Nothing at all in the history available, which goes back this far.
    QuietSince {
        checked_back_to: Date,
    },
    Unknown {
        reason: String,
    },
}

impl ActivityStatus {
    pub fn label(&self) -> &'static str {
        match self {
            ActivityStatus::Active { .. } => "Active",
            ActivityStatus::Quiet { .. } | ActivityStatus::QuietSince { .. } => "Quiet",
            ActivityStatus::Unknown { .. } => "Unknown",
        }
    }

    /// A human-readable description of the last known activity.
    pub fn detail(&self) -> String {
        match self {
            ActivityStatus::Active { last_change } | ActivityStatus::Quiet { last_change } => {
                last_change.to_string()
            }
            ActivityStatus::QuietSince { checked_back_to } => {
                format!("None found back to {checked_back_to}")
            }
            ActivityStatus::Unknown { reason } => reason.clone(),
        }
    }
}

/// A package together with its verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageStatus {
    pub package: InstalledPackage,
    pub status: MaintainerStatus,
}

#[cfg(test)]
mod tests {
    use time::Month;

    use super::*;

    #[test]
    fn orphaned_reason_includes_both_dates_when_known() {
        let status = MaintainerStatus::Orphaned {
            since: Some(Date::from_calendar_date(2026, Month::September, 30).unwrap()),
        };
        assert_eq!(
            status.reason(),
            "No maintainer since 2026-09-30; retirement due around 2026-11-11 unless adopted"
        );
    }

    #[test]
    fn retired_reason_is_passed_through_unchanged() {
        let status = MaintainerStatus::Retired {
            reason: "Dead since kernel 7.2".to_string(),
        };
        assert_eq!(status.label(), "Retired");
        assert_eq!(status.reason(), "Dead since kernel 7.2");
    }
}
