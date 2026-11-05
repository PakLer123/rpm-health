//! Signal 1: does this package still have a maintainer, and is it still being
//! shipped? Combines the data sources into one status per package.

use std::collections::HashMap;

use crate::error::Result;
use crate::http::Transport;
use crate::model::{InstalledPackage, MaintainerStatus, PackageStatus};
use crate::sources::koji::KojiPackage;
use crate::sources::{distgit, koji, orphans};

const RAWHIDE_BRANCH: &str = "rawhide";

/// The outcome of checking a set of installed packages.
#[derive(Debug)]
pub struct MaintainerReport {
    /// One entry per Fedora package, sorted by name.
    pub statuses: Vec<PackageStatus>,
    /// Fedora's data says nothing about these, so they aren't checked.
    pub third_party: Vec<InstalledPackage>,
    /// Problems that made the report less complete without stopping it.
    pub warnings: Vec<String>,
}

/// What Koji's data alone says, before any follow-up lookups.
#[derive(Debug, PartialEq, Eq)]
enum Classification {
    Retired,
    Retiring,
    Orphaned,
    Maintained,
    NotFound,
}

/// The core rule. It's a pure function, with no network, so it's easy to test.
///
/// `current` is this system's release, and `next` is Rawhide. The most severe
/// finding wins. Ownership is read from Rawhide when possible because that's
/// the most recent view of who looks after the package.
fn classify(current: Option<&KojiPackage>, next: Option<&KojiPackage>) -> Classification {
    if current.is_some_and(|package| package.blocked) {
        Classification::Retired
    } else if next.is_some_and(|package| package.blocked) {
        Classification::Retiring
    } else {
        match next.or(current) {
            Some(package) if package.is_orphaned() => Classification::Orphaned,
            Some(_) => Classification::Maintained,
            None => Classification::NotFound,
        }
    }
}

/// Checks packages against Fedora's data. `current_tag` is this system's Koji
/// tag, such as `f45`. Without `rawhide_tag`, upcoming retirements can't be checked.
///
/// Koji is required, because without it there's nothing to report. The other
/// sources add detail, so if one fails we carry on and record a warning.
pub fn check_maintainers(
    transport: &dyn Transport,
    packages: Vec<InstalledPackage>,
    current_tag: &str,
    rawhide_tag: Option<&str>,
) -> Result<MaintainerReport> {
    // `partition` moves each package into one of two new lists. `packages` is
    // used up by this, which the compiler enforces: touching it later wouldn't
    // compile.
    let (fedora, third_party): (Vec<_>, Vec<_>) = packages.into_iter().partition(|p| p.from_fedora);
    let names: Vec<String> = fedora.iter().map(|p| p.source_name.clone()).collect();
    let mut warnings = Vec::new();

    let current = koji::list_packages(transport, current_tag, &names)?;

    // On Rawhide itself there's no "next release" to compare against.
    let next = match rawhide_tag {
        Some(tag) if tag != current_tag => koji::list_packages(transport, tag, &names)?,
        _ => HashMap::new(),
    };

    let orphan_dates = orphans::fetch_orphan_dates(transport).unwrap_or_else(|e| {
        warnings.push(format!("Orphan dates are unavailable: {e}"));
        HashMap::new()
    });

    let statuses = fedora
        .into_iter()
        .map(|package| {
            let name = package.source_name.as_str();
            let status = match classify(current.get(name), next.get(name)) {
                Classification::Retired => {
                    // A package retired before this release branched has no
                    // branch for this release at all, so also try Rawhide.
                    confirm_retirement(
                        transport,
                        name,
                        current_tag,
                        &[current_tag, RAWHIDE_BRANCH],
                        |reason| MaintainerStatus::Retired { reason },
                        &mut warnings,
                    )
                }
                Classification::Retiring => confirm_retirement(
                    transport,
                    name,
                    RAWHIDE_BRANCH,
                    &[RAWHIDE_BRANCH],
                    |reason| MaintainerStatus::Retiring { reason },
                    &mut warnings,
                ),
                Classification::Orphaned => MaintainerStatus::Orphaned {
                    since: orphan_dates.get(name).copied(),
                },
                Classification::Maintained => MaintainerStatus::Maintained,
                Classification::NotFound => MaintainerStatus::Unknown {
                    reason: format!("Not found in Koji for {current_tag}"),
                },
            };
            PackageStatus { package, status }
        })
        .collect();

    Ok(MaintainerReport {
        statuses,
        third_party,
        warnings,
    })
}

/// Koji's `blocked` flag alone doesn't prove retirement, so look for the
/// `dead.package` note. Without one the honest answer is "unknown".
///
/// `make_status` is a closure, a function passed as a value. It lets this code
/// be shared while the caller decides whether the result is Retired or Retiring.
fn confirm_retirement(
    transport: &dyn Transport,
    package: &str,
    blocked_in: &str,
    branches: &[&str],
    make_status: impl Fn(String) -> MaintainerStatus,
    warnings: &mut Vec<String>,
) -> MaintainerStatus {
    for branch in branches {
        match distgit::dead_package_note(transport, package, branch) {
            Ok(Some(note)) if note.is_empty() => {
                return make_status("Retired; no reason recorded".to_string());
            }
            Ok(Some(note)) => return make_status(note),
            Ok(None) => {}
            Err(e) => {
                warnings.push(format!("Couldn't confirm that {package} is retired: {e}"));
                return MaintainerStatus::Unknown {
                    reason: format!("Blocked in {blocked_in}, but the retirement check failed"),
                };
            }
        }
    }
    MaintainerStatus::Unknown {
        reason: format!("Blocked in {blocked_in}, but no retirement note was found"),
    }
}

#[cfg(test)]
mod tests {
    use time::{Date, Month};

    use super::*;
    use crate::error::Error;
    use crate::http::fake::FakeTransport;

    fn koji_package(blocked: bool, owner: &str) -> KojiPackage {
        KojiPackage {
            blocked,
            owner: owner.to_string(),
        }
    }

    fn installed(name: &str, from_fedora: bool) -> InstalledPackage {
        InstalledPackage {
            source_name: name.to_string(),
            binaries: vec![name.to_string()],
            versions: vec!["1.0-1.fc45".to_string()],
            from_fedora,
            fedora_version: None,
            changelog: Vec::new(),
        }
    }

    /// One multicall answer per (blocked, owner) pair. `None` means Koji
    /// doesn't know the package.
    fn koji_reply(rows: &[Option<(bool, &str)>]) -> String {
        let values: String = rows
            .iter()
            .map(|row| match row {
                Some((blocked, owner)) => format!(
                    "<value><array><data><value><array><data><value><struct>\
                     <member><name>blocked</name><value><boolean>{}</boolean></value></member>\
                     <member><name>owner_name</name><value><string>{owner}</string></value></member>\
                     </struct></value></data></array></value></data></array></value>",
                    u8::from(*blocked)
                ),
                None => "<value><struct><member><name>faultString</name>\
                         <value><string>No such entry in table package: x</string></value>\
                         </member></struct></value>"
                    .to_string(),
            })
            .collect();
        format!(
            "<methodResponse><params><param><value><array><data>{values}</data></array></value>\
             </param></params></methodResponse>"
        )
    }

    const NO_ORPHANS: &str = "{\"status_change\": {}}";

    /// A transport where the orphan report works.
    fn working_transport() -> FakeTransport {
        FakeTransport::default().with_get(orphans::REPORT_URL, NO_ORPHANS)
    }

    #[test]
    fn classify_prefers_the_most_severe_finding() {
        let retired = koji_package(true, "orphan");
        let orphaned = koji_package(false, "orphan");
        let owned = koji_package(false, "alice");
        let blocked_owned = koji_package(true, "alice");

        assert_eq!(
            classify(Some(&retired), Some(&retired)),
            Classification::Retired
        );
        // Blocked with a real owner is what linux-atm looks like.
        assert_eq!(
            classify(Some(&blocked_owned), Some(&blocked_owned)),
            Classification::Retired
        );
        assert_eq!(
            classify(Some(&owned), Some(&blocked_owned)),
            Classification::Retiring
        );
        assert_eq!(
            classify(Some(&orphaned), Some(&orphaned)),
            Classification::Orphaned
        );
        assert_eq!(
            classify(Some(&owned), Some(&owned)),
            Classification::Maintained
        );
        assert_eq!(classify(None, None), Classification::NotFound);
    }

    #[test]
    fn classify_uses_rawhide_for_ownership_and_copes_without_it() {
        let orphaned = koji_package(false, "orphan");
        let owned = koji_package(false, "alice");
        // Adopted in Rawhide, while the older tag still says orphan.
        assert_eq!(
            classify(Some(&orphaned), Some(&owned)),
            Classification::Maintained
        );
        assert_eq!(classify(Some(&orphaned), None), Classification::Orphaned);
    }

    #[test]
    fn third_party_packages_are_set_aside_without_being_queried() {
        let transport = working_transport()
            .with_post_reply(&koji_reply(&[Some((false, "alice"))]))
            .with_post_reply(&koji_reply(&[Some((false, "alice"))]));
        let packages = vec![installed("htop", true), installed("dbeaver-ce", false)];

        let report = check_maintainers(&transport, packages, "f45", Some("f46")).unwrap();

        assert_eq!(report.statuses.len(), 1);
        assert_eq!(report.third_party[0].source_name, "dbeaver-ce");
        let requests = transport.post_bodies.borrow();
        assert!(requests.iter().all(|body| !body.contains("dbeaver-ce")));
    }

    #[test]
    fn retired_package_gets_its_note_from_the_release_branch() {
        let transport = working_transport()
            .with_post_reply(&koji_reply(&[Some((true, "alice"))]))
            .with_post_reply(&koji_reply(&[Some((true, "alice"))]))
            .with_get(
                &distgit::dead_package_url("linux-atm", "f45"),
                "Dead since kernel 7.2\n",
            );

        let report = check_maintainers(
            &transport,
            vec![installed("linux-atm", true)],
            "f45",
            Some("f46"),
        )
        .unwrap();

        assert_eq!(
            report.statuses[0].status,
            MaintainerStatus::Retired {
                reason: "Dead since kernel 7.2".to_string()
            }
        );
    }

    /// Real behaviour: 389-admin has no dead.package on f45, only on rawhide.
    #[test]
    fn retired_package_falls_back_to_rawhide_for_its_note() {
        let transport = working_transport()
            .with_post_reply(&koji_reply(&[Some((true, "orphan"))]))
            .with_post_reply(&koji_reply(&[Some((true, "orphan"))]))
            .with_get(
                &distgit::dead_package_url("389-admin", "rawhide"),
                "Replaced by Cockpit",
            );

        let report = check_maintainers(
            &transport,
            vec![installed("389-admin", true)],
            "f45",
            Some("f46"),
        )
        .unwrap();

        assert_eq!(
            report.statuses[0].status,
            MaintainerStatus::Retired {
                reason: "Replaced by Cockpit".to_string()
            }
        );
    }

    #[test]
    fn blocked_without_a_note_is_unknown_rather_than_retired() {
        let transport = working_transport()
            .with_post_reply(&koji_reply(&[Some((true, "alice"))]))
            .with_post_reply(&koji_reply(&[Some((true, "alice"))]));

        let report =
            check_maintainers(&transport, vec![installed("odd", true)], "f45", Some("f46"))
                .unwrap();

        assert_eq!(report.statuses[0].status.label(), "Unknown");
    }

    #[test]
    fn blocked_only_in_rawhide_is_retiring() {
        let transport = working_transport()
            .with_post_reply(&koji_reply(&[Some((false, "alice"))]))
            .with_post_reply(&koji_reply(&[Some((true, "alice"))]))
            .with_get(&distgit::dead_package_url("old", "rawhide"), "Unmaintained");

        let report =
            check_maintainers(&transport, vec![installed("old", true)], "f45", Some("f46"))
                .unwrap();

        assert_eq!(
            report.statuses[0].status,
            MaintainerStatus::Retiring {
                reason: "Unmaintained".to_string()
            }
        );
    }

    #[test]
    fn orphaned_package_gets_its_date_from_the_report() {
        let report_json = "{\"status_change\": {\"lonely\": \"2026-10-26T21:47:09+00:00\"}}";
        let transport = FakeTransport::default()
            .with_get(orphans::REPORT_URL, report_json)
            .with_post_reply(&koji_reply(&[Some((false, "orphan"))]))
            .with_post_reply(&koji_reply(&[Some((false, "orphan"))]));

        let report = check_maintainers(
            &transport,
            vec![installed("lonely", true)],
            "f45",
            Some("f46"),
        )
        .unwrap();

        assert_eq!(
            report.statuses[0].status,
            MaintainerStatus::Orphaned {
                since: Some(Date::from_calendar_date(2026, Month::October, 26).unwrap())
            }
        );
    }

    /// Real behaviour: the report also lists packages that have an owner in
    /// Koji. Koji decides, and the report only supplies dates.
    #[test]
    fn listing_in_the_report_alone_does_not_make_a_package_orphaned() {
        let report_json = "{\"status_change\": {\"links\": \"2026-09-30T17:48:50+00:00\"}}";
        let transport = FakeTransport::default()
            .with_get(orphans::REPORT_URL, report_json)
            .with_post_reply(&koji_reply(&[Some((false, "alice"))]))
            .with_post_reply(&koji_reply(&[Some((false, "alice"))]));

        let report = check_maintainers(
            &transport,
            vec![installed("links", true)],
            "f45",
            Some("f46"),
        )
        .unwrap();

        assert_eq!(report.statuses[0].status, MaintainerStatus::Maintained);
    }

    #[test]
    fn package_unknown_to_koji_is_unknown() {
        let transport = working_transport()
            .with_post_reply(&koji_reply(&[None]))
            .with_post_reply(&koji_reply(&[None]));

        let report = check_maintainers(
            &transport,
            vec![installed("mine", true)],
            "f45",
            Some("f46"),
        )
        .unwrap();

        assert_eq!(report.statuses[0].status.label(), "Unknown");
    }

    #[test]
    fn scan_continues_with_a_warning_when_the_orphan_report_fails() {
        let transport = FakeTransport::default()
            .with_failing_get(orphans::REPORT_URL)
            .with_post_reply(&koji_reply(&[Some((false, "orphan"))]));

        let report =
            check_maintainers(&transport, vec![installed("lonely", true)], "f45", None).unwrap();

        assert_eq!(report.warnings.len(), 1);
        assert_eq!(
            report.statuses[0].status,
            MaintainerStatus::Orphaned { since: None }
        );
    }

    #[test]
    fn failed_retirement_lookup_warns_and_reports_unknown() {
        let transport = working_transport()
            .with_post_reply(&koji_reply(&[Some((true, "alice"))]))
            .with_post_reply(&koji_reply(&[Some((true, "alice"))]))
            .with_failing_get(&distgit::dead_package_url("flaky", "f45"));

        let report = check_maintainers(
            &transport,
            vec![installed("flaky", true)],
            "f45",
            Some("f46"),
        )
        .unwrap();

        assert_eq!(report.statuses[0].status.label(), "Unknown");
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn on_rawhide_koji_is_only_queried_once() {
        let transport = working_transport().with_post_reply(&koji_reply(&[Some((false, "alice"))]));

        let report = check_maintainers(
            &transport,
            vec![installed("htop", true)],
            "f46",
            Some("f46"),
        )
        .unwrap();

        assert_eq!(report.statuses[0].status, MaintainerStatus::Maintained);
        assert_eq!(transport.post_bodies.borrow().len(), 1);
    }

    #[test]
    fn koji_failure_stops_the_scan() {
        let bad_reply = "<methodResponse><params></params></methodResponse>";
        let transport = working_transport().with_post_reply(bad_reply);

        let result = check_maintainers(
            &transport,
            vec![installed("htop", true)],
            "f45",
            Some("f46"),
        );

        assert!(matches!(result, Err(Error::Parse { .. })));
    }
}
