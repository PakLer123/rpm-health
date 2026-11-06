//! Combines the three signals into one verdict per package.
//!
//! No single signal decides on its own unless it's definitive. Old doesn't mean
//! unhealthy, because a finished, stable tool can go years without changes, so a
//! quiet package that matches upstream is healthy.

use serde::Serialize;
use time::Date;

use crate::error::Result;
use crate::http::Transport;
use crate::model::{ActivityStatus, InstalledPackage, MaintainerStatus, UpstreamStatus};
use crate::signals::activity::check_activity;
use crate::signals::maintainer::check_maintainers;
use crate::signals::upstream::check_upstream;
use crate::sources::bodhi;

/// The overall verdict for a package, most urgent first. `derive(Ord)` follows
/// this order, so sorting by label puts the most urgent packages at the top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthLabel {
    Retired,
    Orphaned,
    AtRisk,
    Stale,
    Unknown,
    Healthy,
}

impl HealthLabel {
    pub fn name(self) -> &'static str {
        match self {
            HealthLabel::Healthy => "Healthy",
            HealthLabel::Stale => "Stale",
            HealthLabel::AtRisk => "At risk",
            HealthLabel::Orphaned => "Orphaned",
            HealthLabel::Retired => "Retired",
            HealthLabel::Unknown => "Unknown",
        }
    }
}

/// Everything we know about one package.
#[derive(Debug, Serialize)]
pub struct PackageHealth {
    pub package: InstalledPackage,
    pub label: HealthLabel,
    pub reason: String,
    pub maintainer: MaintainerStatus,
    pub activity: ActivityStatus,
    pub upstream: UpstreamStatus,
}

/// Bumped whenever the JSON output changes in a way that could break scripts.
pub const JSON_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
pub struct HealthReport {
    pub schema_version: u32,
    pub release: String,
    /// One entry per Fedora package, sorted by name.
    pub packages: Vec<PackageHealth>,
    /// Fedora's data says nothing about these, so they aren't checked.
    pub third_party: Vec<InstalledPackage>,
    /// Problems that made the report less complete without stopping it.
    pub warnings: Vec<String>,
}

/// Runs every signal and combines the results. This is the main entry point for
/// front-ends. `current_tag` is this system's Koji tag, such as `f45`.
pub fn check_health(
    transport: &dyn Transport,
    packages: Vec<InstalledPackage>,
    current_tag: &str,
    today: Date,
) -> Result<HealthReport> {
    let mut warnings = Vec::new();
    let rawhide_tag = match bodhi::rawhide_tag(transport) {
        Ok(tag) => Some(tag),
        Err(e) => {
            warnings.push(format!(
                "Skipped the checks against Rawhide, because its release is unknown: {e}"
            ));
            None
        }
    };

    let maintainers = check_maintainers(transport, packages, current_tag, rawhide_tag.as_deref())?;
    warnings.extend(maintainers.warnings);

    // These reports borrow from `maintainers.statuses`. Taking just the statuses
    // out ends that borrow, so the packages can be moved into the results below.
    let fedora: Vec<&InstalledPackage> = maintainers
        .statuses
        .iter()
        .map(|entry| &entry.package)
        .collect();
    let activity = check_activity(transport, &fedora, today);
    warnings.extend(activity.warnings);
    let activities: Vec<ActivityStatus> = activity.statuses.into_iter().map(|e| e.status).collect();
    let upstream = check_upstream(transport, &fedora, rawhide_tag.as_deref());
    warnings.extend(upstream.warnings);

    // `zip` walks the three lists in step. All of them follow the same order.
    let packages = maintainers
        .statuses
        .into_iter()
        .zip(activities)
        .zip(upstream.statuses)
        .map(|((entry, activity), upstream)| {
            let (label, reason) = assess(&entry.status, &activity, &upstream, today);
            PackageHealth {
                package: entry.package,
                label,
                reason,
                maintainer: entry.status,
                activity,
                upstream,
            }
        })
        .collect();

    Ok(HealthReport {
        schema_version: JSON_SCHEMA_VERSION,
        release: current_tag.to_string(),
        packages,
        third_party: maintainers.third_party,
        warnings,
    })
}

/// The labelling rules, as a pure function.
pub fn assess(
    maintainer: &MaintainerStatus,
    activity: &ActivityStatus,
    upstream: &UpstreamStatus,
    today: Date,
) -> (HealthLabel, String) {
    match maintainer {
        MaintainerStatus::Retired { .. } => (HealthLabel::Retired, maintainer.reason()),
        MaintainerStatus::Retiring { reason } => (
            HealthLabel::AtRisk,
            format!("Retired in Rawhide, so it won't be in the next release: {reason}"),
        ),
        MaintainerStatus::Orphaned { .. } => (HealthLabel::Orphaned, maintainer.reason()),
        MaintainerStatus::Unknown { .. } => (HealthLabel::Unknown, maintainer.reason()),
        MaintainerStatus::Maintained => assess_maintained(activity, upstream, today),
    }
}

fn assess_maintained(
    activity: &ActivityStatus,
    upstream: &UpstreamStatus,
    today: Date,
) -> (HealthLabel, String) {
    if upstream.is_well_behind(today) {
        return (
            HealthLabel::Stale,
            format!("A major version behind. {}", upstream.detail()),
        );
    }

    let is_up_to_date = matches!(upstream, UpstreamStatus::UpToDate { .. });
    let is_behind = matches!(upstream, UpstreamStatus::Behind { .. });

    match activity {
        ActivityStatus::Active { .. } => {
            let update = format!("Updated {}", activity.detail());
            (
                HealthLabel::Healthy,
                format!("{update}. {}", upstream.detail()),
            )
        }
        quiet if quiet.is_quiet() => {
            let history = quiet_history(quiet);
            if is_up_to_date {
                (
                    HealthLabel::Healthy,
                    format!("{history}, but it matches upstream, so it may just be finished"),
                )
            } else if is_behind {
                (
                    HealthLabel::Stale,
                    format!("{history}. {}", upstream.detail()),
                )
            } else {
                (
                    HealthLabel::Unknown,
                    format!("{history}. {}", upstream.detail()),
                )
            }
        }
        _ if is_up_to_date => (
            HealthLabel::Healthy,
            format!("{}. Activity is unknown", upstream.detail()),
        ),
        _ => (
            HealthLabel::Unknown,
            format!("{}. {}", activity.detail(), upstream.detail()),
        ),
    }
}

fn quiet_history(activity: &ActivityStatus) -> String {
    match activity {
        ActivityStatus::Quiet { last_change } => format!("No changes since {last_change}"),
        ActivityStatus::QuietSince { checked_back_to } => {
            format!("No changes found back to {checked_back_to}")
        }
        other => other.detail(),
    }
}

#[cfg(test)]
mod tests {
    use time::Month;

    use super::*;
    use crate::http::fake::FakeTransport;
    use crate::version::Gap;

    fn date() -> Date {
        Date::from_calendar_date(2024, Month::May, 25).unwrap()
    }

    fn active() -> ActivityStatus {
        ActivityStatus::Active {
            last_change: date(),
        }
    }

    fn quiet() -> ActivityStatus {
        ActivityStatus::Quiet {
            last_change: date(),
        }
    }

    fn unknown_activity() -> ActivityStatus {
        ActivityStatus::Unknown {
            reason: "No history".to_string(),
        }
    }

    fn up_to_date() -> UpstreamStatus {
        UpstreamStatus::UpToDate {
            upstream: "1.0".to_string(),
        }
    }

    fn behind(fedora_gap: Option<Gap>) -> UpstreamStatus {
        UpstreamStatus::Behind {
            upstream: "2.0".to_string(),
            gap: Gap::Major,
            rawhide: None,
            fedora_gap,
            new_major_since: None,
        }
    }

    fn today() -> Date {
        Date::from_calendar_date(2026, Month::November, 6).unwrap()
    }

    fn major_gap_since(days_ago: i64) -> UpstreamStatus {
        UpstreamStatus::Behind {
            upstream: "2.0".to_string(),
            gap: Gap::Major,
            rawhide: Some("1.0".to_string()),
            fedora_gap: Some(Gap::Major),
            new_major_since: Some(today() - time::Duration::days(days_ago)),
        }
    }

    fn label(
        maintainer: &MaintainerStatus,
        activity: &ActivityStatus,
        upstream: &UpstreamStatus,
    ) -> HealthLabel {
        assess(maintainer, activity, upstream, today()).0
    }

    fn maintained(activity: &ActivityStatus, upstream: &UpstreamStatus) -> HealthLabel {
        label(&MaintainerStatus::Maintained, activity, upstream)
    }

    #[test]
    fn maintainer_problems_outrank_every_other_signal() {
        let healthy_otherwise = (active(), up_to_date());
        let cases = [
            (
                MaintainerStatus::Retired {
                    reason: "Dead".to_string(),
                },
                HealthLabel::Retired,
            ),
            (
                MaintainerStatus::Retiring {
                    reason: "Dead".to_string(),
                },
                HealthLabel::AtRisk,
            ),
            (
                MaintainerStatus::Orphaned { since: None },
                HealthLabel::Orphaned,
            ),
            (
                MaintainerStatus::Unknown {
                    reason: "?".to_string(),
                },
                HealthLabel::Unknown,
            ),
        ];
        for (maintainer, expected) in cases {
            assert_eq!(
                label(&maintainer, &healthy_otherwise.0, &healthy_otherwise.1),
                expected
            );
        }
    }

    /// The case from the project brief: old, but finished and stable.
    #[test]
    fn quiet_but_matching_upstream_is_healthy() {
        let (label, reason) = assess(
            &MaintainerStatus::Maintained,
            &quiet(),
            &up_to_date(),
            today(),
        );
        assert_eq!(label, HealthLabel::Healthy);
        assert!(reason.contains("2024-05-25"));
    }

    #[test]
    fn quiet_and_behind_is_stale() {
        assert_eq!(
            maintained(&quiet(), &behind(Some(Gap::Patch))),
            HealthLabel::Stale
        );
    }

    #[test]
    fn quiet_with_no_upstream_data_is_unknown_not_healthy_or_stale() {
        assert_eq!(
            maintained(&quiet(), &UpstreamStatus::NotTracked),
            HealthLabel::Unknown
        );
    }

    #[test]
    fn active_and_slightly_behind_is_healthy() {
        assert_eq!(
            maintained(&active(), &behind(Some(Gap::Minor))),
            HealthLabel::Healthy
        );
    }

    #[test]
    fn active_with_no_upstream_data_is_healthy() {
        assert_eq!(
            maintained(&active(), &UpstreamStatus::NotTracked),
            HealthLabel::Healthy
        );
    }

    #[test]
    fn a_major_gap_in_fedora_is_stale_even_when_active() {
        assert_eq!(
            maintained(&active(), &behind(Some(Gap::Major))),
            HealthLabel::Stale
        );
    }

    /// Real case: filelock's new major is 50 days old, and the package is active.
    #[test]
    fn a_recent_major_version_does_not_make_an_active_package_stale() {
        let (label, reason) = assess(
            &MaintainerStatus::Maintained,
            &active(),
            &major_gap_since(50),
            today(),
        );
        assert_eq!(label, HealthLabel::Healthy);
        assert!(reason.contains("The first newer major version appeared on"));
    }

    #[test]
    fn an_old_major_version_makes_an_active_package_stale() {
        assert_eq!(
            maintained(&active(), &major_gap_since(90)),
            HealthLabel::Stale
        );
    }

    /// The grace period only protects active packages. A package nobody has
    /// touched for two years is still stale once upstream has moved on.
    #[test]
    fn a_recent_major_version_does_not_excuse_a_quiet_package() {
        assert_eq!(
            maintained(&quiet(), &major_gap_since(50)),
            HealthLabel::Stale
        );
    }

    /// A new major version in Rawhide is normal, not a sign of neglect.
    #[test]
    fn a_major_gap_that_rawhide_closed_is_not_stale() {
        assert_eq!(maintained(&active(), &behind(None)), HealthLabel::Healthy);
    }

    #[test]
    fn unknown_activity_only_gets_a_verdict_from_clear_upstream_data() {
        assert_eq!(
            maintained(&unknown_activity(), &up_to_date()),
            HealthLabel::Healthy
        );
        assert_eq!(
            maintained(&unknown_activity(), &behind(Some(Gap::Major))),
            HealthLabel::Stale
        );
        assert_eq!(
            maintained(&unknown_activity(), &behind(Some(Gap::Minor))),
            HealthLabel::Unknown
        );
        assert_eq!(
            maintained(&unknown_activity(), &UpstreamStatus::NotTracked),
            HealthLabel::Unknown
        );
    }

    #[test]
    fn a_failed_upstream_check_never_makes_a_quiet_package_stale() {
        let not_checked = UpstreamStatus::NotChecked {
            reason: "Offline".to_string(),
        };
        assert_eq!(maintained(&quiet(), &not_checked), HealthLabel::Unknown);
    }

    #[test]
    fn labels_sort_most_urgent_first() {
        assert!(HealthLabel::Retired < HealthLabel::Stale);
        assert!(HealthLabel::Stale < HealthLabel::Healthy);
    }

    /// Runs the whole flow with every optional source down. The scan should
    /// still finish, explain what's missing, and not invent a verdict.
    #[test]
    fn check_health_survives_optional_sources_failing() {
        let package = InstalledPackage {
            source_name: "old".to_string(),
            binaries: vec!["old".to_string()],
            versions: vec!["1.0-1.fc45".to_string()],
            from_fedora: true,
            fedora_version: Some("1.0".to_string()),
            changelog: Vec::new(),
        };
        let third_party = InstalledPackage {
            source_name: "mine".to_string(),
            from_fedora: false,
            ..package.clone()
        };
        let koji_row = "<methodResponse><params><param><value><array><data>\
            <value><array><data><value><array><data><value><struct>\
            <member><name>blocked</name><value><boolean>0</boolean></value></member>\
            <member><name>owner_name</name><value><string>alice</string></value></member>\
            </struct></value></data></array></value></data></array></value>\
            </data></array></value></param></params></methodResponse>";
        let no_builds = "<methodResponse><params><param><value><array><data>\
            <value><array><data><value><array><data></data></array></value></data></array></value>\
            </data></array></value></param></params></methodResponse>";
        // The Koji package lookup, then the build lookup for the quiet package.
        let transport = FakeTransport::default()
            .with_post_reply(koji_row)
            .with_post_reply(no_builds);

        let report = check_health(&transport, vec![package, third_party], "f45", date()).unwrap();

        assert_eq!(report.packages.len(), 1);
        assert_eq!(report.packages[0].label, HealthLabel::Unknown);
        assert_eq!(report.third_party.len(), 1);
        // Bodhi, the orphan report, and release-monitoring.org.
        assert_eq!(report.warnings.len(), 3);
    }

    #[test]
    fn json_output_has_a_stable_readable_shape() {
        let entry = PackageHealth {
            package: InstalledPackage {
                source_name: "demo".to_string(),
                binaries: vec!["demo-libs".to_string()],
                versions: vec!["1.0-1.fc45".to_string()],
                from_fedora: true,
                fedora_version: Some("1.0".to_string()),
                changelog: vec![crate::model::ChangelogEntry {
                    date: date(),
                    author: "Alice".to_string(),
                    text: "- Secret".to_string(),
                }],
            },
            label: HealthLabel::AtRisk,
            reason: "Because".to_string(),
            maintainer: MaintainerStatus::Maintained,
            activity: quiet(),
            upstream: major_gap_since(50),
        };

        let json = serde_json::to_value(&entry).unwrap();

        assert_eq!(json["label"], "at_risk");
        assert_eq!(json["maintainer"]["status"], "maintained");
        assert_eq!(json["activity"]["status"], "quiet");
        assert_eq!(json["activity"]["last_change"], "2024-05-25");
        assert_eq!(json["upstream"]["fedora_gap"], "major");
        assert_eq!(json["upstream"]["new_major_since"], "2026-09-17");
        assert_eq!(json["package"]["source_name"], "demo");
        assert!(json["package"].get("changelog").is_none());
    }
}
