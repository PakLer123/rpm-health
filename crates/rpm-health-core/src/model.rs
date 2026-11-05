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
    /// `since` is a `YYYY-MM-DD` date, when the orphan report provides one.
    Orphaned {
        since: Option<String>,
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
                format!("No maintainer since {date}; retired after six weeks unless adopted")
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

/// A package together with its verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageStatus {
    pub package: InstalledPackage,
    pub status: MaintainerStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orphaned_reason_includes_the_date_when_known() {
        let status = MaintainerStatus::Orphaned {
            since: Some("2026-09-30".to_string()),
        };
        assert!(status.reason().contains("2026-09-30"));
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
