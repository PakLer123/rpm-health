//! Signal 2: has anything happened to this package recently, other than the
//! mass rebuilds Fedora does for every package twice a year?
//!
//! Installed RPMs carry their own changelog, so most packages can be answered
//! with no network at all. Koji is only asked about the ones that look quiet,
//! because it can see builds newer than what's installed, and further back than
//! the two or three years of history that changelogs keep.
//!
//! The rule is deliberately lenient. A rebuild for a new Python or library
//! version counts as activity, because Koji can't tell those apart from other
//! work. So some "Active" packages haven't had a maintainer's attention, but we
//! avoid calling a package "Quiet" when someone has recently worked on it.

use std::collections::HashMap;

use time::{Date, Duration};

use crate::http::Transport;
use crate::model::{ActivityStatus, ChangelogEntry, InstalledPackage};
use crate::sources::koji::{self, KojiBuild};

/// About four Fedora releases, and within what changelogs reliably keep.
const QUIET_AFTER: Duration = Duration::days(730);
const RELENG_EMAIL: &str = "releng@fedoraproject.org";
/// The Koji user that runs mass rebuilds.
const RELENG_OWNER: &str = "releng";

/// A package and its activity, borrowed from wherever the package list lives.
///
/// `'a` is a lifetime. It tells the compiler this struct holds a reference, and
/// must not outlive the package it points to. Borrowing here means we don't copy
/// every package's changelog just to report on it.
#[derive(Debug)]
pub struct PackageActivity<'a> {
    pub package: &'a InstalledPackage,
    pub status: ActivityStatus,
}

#[derive(Debug)]
pub struct ActivityReport<'a> {
    pub statuses: Vec<PackageActivity<'a>>,
    /// Problems that made the report less complete without stopping it.
    pub warnings: Vec<String>,
}

/// Checks when each package last changed. `today` is a parameter, not read from
/// the clock in here, so tests can use a fixed date.
pub fn check_activity<'a>(
    transport: &dyn Transport,
    packages: &[&'a InstalledPackage],
    today: Date,
) -> ActivityReport<'a> {
    let cutoff = today - QUIET_AFTER;
    let looks_quiet = |package: &InstalledPackage| {
        last_local_change(&package.changelog).is_none_or(|date| date < cutoff)
    };

    let quiet_names: Vec<String> = packages
        .iter()
        .filter(|package| looks_quiet(package))
        .map(|package| package.source_name.clone())
        .collect();

    let mut warnings = Vec::new();
    let builds = match koji::recent_builds(transport, &quiet_names) {
        Ok(builds) => Some(builds),
        Err(e) => {
            warnings.push(format!("Couldn't check Koji for quiet packages: {e}"));
            None
        }
    };

    let statuses = packages
        .iter()
        .map(|package| PackageActivity {
            package,
            status: decide(&package.changelog, lookup(&builds, package), cutoff),
        })
        .collect();

    ActivityReport { statuses, warnings }
}

/// What we know from Koji about one package.
enum KojiAnswer<'a> {
    /// We didn't ask, or asking failed.
    NotAvailable,
    Builds(&'a [KojiBuild]),
}

fn lookup<'a>(
    builds: &'a Option<HashMap<String, Vec<KojiBuild>>>,
    package: &InstalledPackage,
) -> KojiAnswer<'a> {
    match builds
        .as_ref()
        .and_then(|found| found.get(&package.source_name))
    {
        Some(builds) => KojiAnswer::Builds(builds),
        None => KojiAnswer::NotAvailable,
    }
}

/// The core rule, as a pure function.
fn decide(changelog: &[ChangelogEntry], koji: KojiAnswer, cutoff: Date) -> ActivityStatus {
    let local = last_local_change(changelog);
    if let Some(date) = local.filter(|date| *date >= cutoff) {
        return ActivityStatus::Active { last_change: date };
    }

    let KojiAnswer::Builds(builds) = koji else {
        return ActivityStatus::Unknown {
            reason: match local {
                Some(date) => format!("Changelog ends at {date}, and Koji couldn't confirm it"),
                None => "No recent changelog entries, and Koji couldn't confirm it".to_string(),
            },
        };
    };

    let koji_change = builds
        .iter()
        .filter(|build| is_maintainer_build(build))
        .map(|build| build.completed)
        .max();

    // `Option`s compare with `None` lowest, so `max` picks the newer known date.
    match local.max(koji_change) {
        Some(date) if date >= cutoff => ActivityStatus::Active { last_change: date },
        Some(date) => ActivityStatus::Quiet { last_change: date },
        None => {
            let oldest_local = changelog.iter().map(|entry| entry.date);
            let oldest_build = builds.iter().map(|build| build.completed);
            match oldest_local.chain(oldest_build).min() {
                Some(date) => ActivityStatus::QuietSince {
                    checked_back_to: date,
                },
                None => ActivityStatus::Unknown {
                    reason: "No changelog or build history found".to_string(),
                },
            }
        }
    }
}

/// The newest entry that isn't a mass rebuild. Changelogs are newest first.
fn last_local_change(changelog: &[ChangelogEntry]) -> Option<Date> {
    changelog
        .iter()
        .find(|entry| !is_mass_rebuild(entry))
        .map(|entry| entry.date)
}

fn is_mass_rebuild(entry: &ChangelogEntry) -> bool {
    let text = entry.text.to_lowercase();
    entry.author.contains(RELENG_EMAIL)
        || text.contains("mass_rebuild")
        || text.contains("mass rebuild")
}

/// Leaves out mass rebuilds. Koji's search pattern already filters most other
/// builds, but it's only a glob, so the release is checked properly here too.
fn is_maintainer_build(build: &KojiBuild) -> bool {
    build.owner != RELENG_OWNER && is_fedora_release(&build.release)
}

fn is_fedora_release(release: &str) -> bool {
    release.rsplit_once(".fc").is_some_and(|(_, version)| {
        !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use time::Month;

    use super::*;
    use crate::http::fake::FakeTransport;

    fn date(year: i32, month: Month, day: u8) -> Date {
        Date::from_calendar_date(year, month, day).unwrap()
    }

    fn today() -> Date {
        date(2026, Month::November, 6)
    }

    fn cutoff() -> Date {
        today() - QUIET_AFTER
    }

    fn entry(date: Date, author: &str, text: &str) -> ChangelogEntry {
        ChangelogEntry {
            date,
            author: author.to_string(),
            text: text.to_string(),
        }
    }

    fn mass_rebuild(date: Date) -> ChangelogEntry {
        entry(
            date,
            "Fedora Release Engineering <releng@fedoraproject.org> - 1-2",
            "- Rebuilt for https://fedoraproject.org/wiki/Fedora_45_Mass_Rebuild",
        )
    }

    fn build(owner: &str, release: &str, completed: Date) -> KojiBuild {
        KojiBuild {
            owner: owner.to_string(),
            release: release.to_string(),
            completed,
        }
    }

    #[test]
    fn a_recent_mass_rebuild_is_not_activity() {
        let changelog = [
            mass_rebuild(date(2026, Month::July, 15)),
            entry(date(2024, Month::May, 25), "Alice", "- Update"),
        ];
        assert_eq!(
            last_local_change(&changelog),
            Some(date(2024, Month::May, 25))
        );
    }

    #[test]
    fn mass_rebuild_text_counts_even_from_another_author() {
        let rebuild = entry(today(), "Alice", "- Rebuilt for Fedora 45 mass rebuild");
        assert!(is_mass_rebuild(&rebuild));
    }

    #[test]
    fn ecosystem_rebuilds_count_as_activity() {
        let rebuild = entry(today(), "Python Maint", "- Rebuilt for Python 3.15");
        assert!(!is_mass_rebuild(&rebuild));
    }

    #[test]
    fn recent_local_change_is_active_without_needing_koji() {
        let last_change = date(2026, Month::October, 1);
        let changelog = [entry(last_change, "Alice", "- Update")];
        assert_eq!(
            decide(&changelog, KojiAnswer::NotAvailable, cutoff()),
            ActivityStatus::Active { last_change }
        );
    }

    #[test]
    fn a_change_exactly_at_the_cutoff_still_counts_as_active() {
        let changelog = [entry(cutoff(), "Alice", "- Update")];
        assert_eq!(
            decide(&changelog, KojiAnswer::NotAvailable, cutoff()),
            ActivityStatus::Active {
                last_change: cutoff()
            }
        );
    }

    /// The installed RPM looks quiet, but Koji has a newer build by a person.
    #[test]
    fn a_recent_koji_build_overrules_a_quiet_changelog() {
        let changelog = [entry(date(2023, Month::March, 1), "Alice", "- Update")];
        let builds = [build("alice", "1.fc45", date(2026, Month::August, 16))];
        assert_eq!(
            decide(&changelog, KojiAnswer::Builds(&builds), cutoff()),
            ActivityStatus::Active {
                last_change: date(2026, Month::August, 16)
            }
        );
    }

    #[test]
    fn koji_finds_a_change_older_than_the_changelog_keeps() {
        let changelog = [mass_rebuild(date(2026, Month::July, 15))];
        let builds = [
            build("releng", "2.fc45", date(2026, Month::July, 15)),
            build("luya", "1.fc41", date(2024, Month::May, 25)),
        ];
        assert_eq!(
            decide(&changelog, KojiAnswer::Builds(&builds), cutoff()),
            ActivityStatus::Quiet {
                last_change: date(2024, Month::May, 25)
            }
        );
    }

    #[test]
    fn only_mass_rebuilds_everywhere_reports_how_far_back_we_looked() {
        let changelog = [mass_rebuild(date(2025, Month::January, 16))];
        let builds = [build("releng", "2.fc45", date(2024, Month::July, 17))];
        assert_eq!(
            decide(&changelog, KojiAnswer::Builds(&builds), cutoff()),
            ActivityStatus::QuietSince {
                checked_back_to: date(2024, Month::July, 17)
            }
        );
    }

    #[test]
    fn flatpak_builds_do_not_count() {
        assert!(is_maintainer_build(&build("a", "1.fc45", today())));
        assert!(!is_maintainer_build(&build("a", "1.fc45app1", today())));
        assert!(!is_maintainer_build(&build("a", "1.eln159", today())));
        assert!(!is_maintainer_build(&build("a", "1.fc", today())));
    }

    #[test]
    fn no_history_at_all_is_unknown() {
        assert_eq!(
            decide(&[], KojiAnswer::Builds(&[]), cutoff()).label(),
            "Unknown"
        );
    }

    fn installed(name: &str, changelog: Vec<ChangelogEntry>) -> InstalledPackage {
        InstalledPackage {
            source_name: name.to_string(),
            binaries: vec![name.to_string()],
            versions: vec!["1-1.fc45".to_string()],
            from_fedora: true,
            fedora_version: None,
            changelog,
        }
    }

    const BUILDS_FIXTURE: &str = include_str!("../../tests/fixtures/koji_list_builds.xml");

    /// Runs the whole flow on a real Koji response. Only the quiet package may
    /// be sent to Koji, because the point is to avoid unnecessary requests.
    #[test]
    fn only_quiet_packages_are_sent_to_koji() {
        let active = installed(
            "busy",
            vec![entry(date(2026, Month::October, 1), "Alice", "- Update")],
        );
        let quiet = installed(
            "aajohan-comfortaa-fonts",
            vec![mass_rebuild(date(2026, Month::July, 15))],
        );
        let other_quiet = installed("rpm-health-no-such-package", Vec::new());
        let transport = FakeTransport::default().with_post_reply(BUILDS_FIXTURE);

        let report = check_activity(&transport, &[&active, &quiet, &other_quiet], today());

        assert!(report.warnings.is_empty());
        assert_eq!(report.statuses[0].status.label(), "Active");
        assert_eq!(
            report.statuses[1].status,
            ActivityStatus::Quiet {
                last_change: date(2024, Month::May, 25)
            }
        );
        assert_eq!(report.statuses[2].status.label(), "Unknown");
        let requests = transport.post_bodies.borrow();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].contains("busy"));
    }

    #[test]
    fn nothing_is_sent_to_koji_when_everything_is_active() {
        let active = installed(
            "busy",
            vec![entry(date(2026, Month::October, 1), "Alice", "- Update")],
        );
        let transport = FakeTransport::default();

        let report = check_activity(&transport, &[&active], today());

        assert!(transport.post_bodies.borrow().is_empty());
        assert_eq!(report.statuses[0].status.label(), "Active");
    }

    #[test]
    fn a_koji_failure_warns_and_leaves_quiet_packages_unknown() {
        let active = installed(
            "busy",
            vec![entry(date(2026, Month::October, 1), "Alice", "- Update")],
        );
        let quiet = installed(
            "old",
            vec![entry(date(2023, Month::March, 1), "Alice", "- Update")],
        );
        let transport = FakeTransport::default().with_post_reply("not xml-rpc");

        let report = check_activity(&transport, &[&active, &quiet], today());

        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.statuses[0].status.label(), "Active");
        assert_eq!(report.statuses[1].status.label(), "Unknown");
        assert!(report.statuses[1].status.detail().contains("2023-03-01"));
    }
}
