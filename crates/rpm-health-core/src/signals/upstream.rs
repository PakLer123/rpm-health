//! Signal 3: how far is Fedora behind the latest upstream release?
//!
//! The installed version is compared with release-monitoring.org first. Packages
//! that look behind are then checked against Rawhide, because a new major version
//! that's waiting there is normal, and not a sign of neglect.

use std::collections::HashMap;

use crate::http::Transport;
use crate::model::{InstalledPackage, UpstreamStatus};
use crate::sources::{anitya, koji};
use crate::version::{Comparison, compare_to_upstream};

#[derive(Debug)]
pub struct UpstreamReport {
    /// One entry per package, in the same order as the packages passed in.
    pub statuses: Vec<UpstreamStatus>,
    /// Problems that made the report less complete without stopping it.
    pub warnings: Vec<String>,
}

pub fn check_upstream(
    transport: &dyn Transport,
    packages: &[&InstalledPackage],
    rawhide_tag: Option<&str>,
) -> UpstreamReport {
    let names: Vec<String> = packages.iter().map(|p| p.source_name.clone()).collect();

    let upstream = match anitya::fetch_upstream_versions(transport, &names) {
        Ok(upstream) => upstream,
        Err(e) => {
            return UpstreamReport {
                statuses: vec![
                    UpstreamStatus::NotChecked {
                        reason: "release-monitoring.org couldn't be reached".to_string()
                    };
                    packages.len()
                ],
                warnings: vec![format!("Skipped the upstream check: {e}")],
            };
        }
    };

    let mut warnings = Vec::new();
    let behind: Vec<String> = packages
        .iter()
        .filter(|package| {
            matches!(
                first_comparison(package, &upstream),
                Some(Comparison::Behind(_))
            )
        })
        .map(|package| package.source_name.clone())
        .collect();
    let rawhide = rawhide_versions(transport, rawhide_tag, &behind, &mut warnings);

    let statuses = packages
        .iter()
        .map(|package| {
            let name = package.source_name.as_str();
            decide(
                name,
                package.fedora_version.as_deref(),
                upstream.get(name),
                rawhide.get(name).map(String::as_str),
            )
        })
        .collect();

    UpstreamReport { statuses, warnings }
}

fn first_comparison(
    package: &InstalledPackage,
    upstream: &HashMap<String, Option<String>>,
) -> Option<Comparison> {
    let installed = package.fedora_version.as_deref()?;
    let latest = upstream.get(&package.source_name)?.as_deref()?;
    Some(compare_to_upstream(&package.source_name, installed, latest))
}

fn rawhide_versions(
    transport: &dyn Transport,
    rawhide_tag: Option<&str>,
    names: &[String],
    warnings: &mut Vec<String>,
) -> HashMap<String, String> {
    let Some(tag) = rawhide_tag else {
        return HashMap::new();
    };
    koji::latest_versions(transport, tag, names).unwrap_or_else(|e| {
        warnings.push(format!("Couldn't check Rawhide's versions: {e}"));
        HashMap::new()
    })
}

/// The core rule, as a pure function. `upstream` is `None` for an untracked
/// package, and `Some(None)` for a tracked one with no stable release.
fn decide(
    name: &str,
    installed: Option<&str>,
    upstream: Option<&Option<String>>,
    rawhide: Option<&str>,
) -> UpstreamStatus {
    let Some(upstream) = upstream else {
        return UpstreamStatus::NotTracked;
    };
    let Some(upstream) = upstream.as_deref() else {
        return UpstreamStatus::NotComparable {
            upstream: None,
            reason: "upstream has no stable release".to_string(),
        };
    };
    let not_comparable = |reason: &str| UpstreamStatus::NotComparable {
        upstream: Some(upstream.to_string()),
        reason: reason.to_string(),
    };
    let Some(installed) = installed else {
        return not_comparable("the installed version is unknown");
    };

    match compare_to_upstream(name, installed, upstream) {
        Comparison::Same => UpstreamStatus::UpToDate {
            upstream: upstream.to_string(),
        },
        Comparison::Ahead => {
            not_comparable("the installed version is newer, so that looks outdated")
        }
        Comparison::NotComparable(reason) => not_comparable(reason),
        Comparison::Behind(gap) => {
            let fedora_gap =
                match rawhide.map(|version| compare_to_upstream(name, version, upstream)) {
                    Some(Comparison::Same | Comparison::Ahead) => None,
                    Some(Comparison::Behind(rawhide_gap)) => Some(rawhide_gap),
                    // Without a usable Rawhide version, the installed one is all we have.
                    Some(Comparison::NotComparable(_)) | None => Some(gap),
                };
            UpstreamStatus::Behind {
                upstream: upstream.to_string(),
                gap,
                rawhide: rawhide.map(str::to_string),
                fedora_gap,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::FakeTransport;
    use crate::version::Gap;

    fn tracked(version: &str) -> Option<String> {
        Some(version.to_string())
    }

    fn behind(
        upstream: &str,
        gap: Gap,
        rawhide: Option<&str>,
        fedora_gap: Option<Gap>,
    ) -> UpstreamStatus {
        UpstreamStatus::Behind {
            upstream: upstream.to_string(),
            gap,
            rawhide: rawhide.map(str::to_string),
            fedora_gap,
        }
    }

    #[test]
    fn untracked_package() {
        assert_eq!(
            decide("x", Some("1.0"), None, None),
            UpstreamStatus::NotTracked
        );
    }

    #[test]
    fn matching_upstream() {
        assert_eq!(
            decide("htop", Some("3.5.3"), Some(&tracked("3.5.3")), None),
            UpstreamStatus::UpToDate {
                upstream: "3.5.3".to_string()
            }
        );
    }

    #[test]
    fn no_stable_release_cannot_be_compared() {
        assert_eq!(
            decide("x", Some("1.0"), Some(&None), None).label(),
            "Can't compare"
        );
    }

    #[test]
    fn outdated_upstream_data_cannot_be_compared() {
        let status = decide("kernel", Some("7.2.15"), Some(&tracked("6.19.14")), None);
        assert_eq!(status.label(), "Can't compare");
        assert!(status.detail().contains("6.19.14"));
    }

    #[test]
    fn rawhide_catching_up_means_fedora_has_no_gap() {
        assert_eq!(
            decide(
                "anaconda",
                Some("45.30"),
                Some(&tracked("46.5")),
                Some("46.5")
            ),
            behind("46.5", Gap::Major, Some("46.5"), None)
        );
    }

    #[test]
    fn rawhide_ahead_of_upstream_data_also_means_no_gap() {
        assert_eq!(
            decide("a", Some("1.0"), Some(&tracked("2.0")), Some("2.1")),
            behind("2.0", Gap::Major, Some("2.1"), None)
        );
    }

    #[test]
    fn a_smaller_gap_in_rawhide_replaces_the_installed_gap() {
        let status = decide("a", Some("1.0.0"), Some(&tracked("2.0.1")), Some("2.0.0"));
        assert_eq!(
            status,
            behind("2.0.1", Gap::Major, Some("2.0.0"), Some(Gap::Patch))
        );
        assert!(!status.is_well_behind());
    }

    #[test]
    fn major_gap_in_rawhide_too_is_well_behind() {
        let status = decide("a", Some("1.0"), Some(&tracked("3.0")), Some("1.1"));
        assert_eq!(
            status,
            behind("3.0", Gap::Major, Some("1.1"), Some(Gap::Major))
        );
        assert!(status.is_well_behind());
    }

    #[test]
    fn without_rawhide_data_the_installed_gap_is_used() {
        let status = decide("a", Some("1.0"), Some(&tracked("2.0")), None);
        assert_eq!(status, behind("2.0", Gap::Major, None, Some(Gap::Major)));
    }

    fn installed(name: &str, version: &str) -> InstalledPackage {
        InstalledPackage {
            source_name: name.to_string(),
            binaries: vec![name.to_string()],
            versions: vec![format!("{version}-1.fc45")],
            from_fedora: true,
            fedora_version: Some(version.to_string()),
            changelog: Vec::new(),
        }
    }

    fn anitya_url(name: &str) -> String {
        format!("https://release-monitoring.org/api/v2/packages/?name={name}&distribution=Fedora")
    }

    fn anitya_body(name: &str, version: &str) -> String {
        format!(
            "{{\"items\": [{{\"name\": \"{name}\", \"stable_version\": \"{version}\"}}], \"total_items\": 1}}"
        )
    }

    const LATEST_FIXTURE: &str = include_str!("../../tests/fixtures/koji_latest_builds.xml");

    #[test]
    fn only_packages_that_look_behind_are_sent_to_koji() {
        let current = installed("current", "1.0");
        let old = installed("old", "1.0");
        let transport = FakeTransport::default()
            .with_get(&anitya_url("current"), &anitya_body("current", "1.0"))
            .with_get(&anitya_url("old"), &anitya_body("old", "2.0"))
            .with_post_reply(
                "<methodResponse><params><param><value><array><data>\
                 <value><array><data><value><array><data></data></array></value></data></array></value>\
                 </data></array></value></param></params></methodResponse>",
            );

        let report = check_upstream(&transport, &[&current, &old], Some("f46"));

        assert_eq!(report.statuses[0].label(), "Up to date");
        assert_eq!(report.statuses[1].label(), "Behind");
        let requests = transport.post_bodies.borrow();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("old"));
        assert!(!requests[0].contains("current"));
    }

    #[test]
    fn reads_rawhide_versions_from_a_real_koji_response() {
        // The fixture answers for three names, so ask about exactly three.
        let packages = [
            installed("htop", "3.0"),
            installed("vim", "9.0"),
            installed("rpm-health-no-such-package", "1.0"),
        ];
        let refs: Vec<&InstalledPackage> = packages.iter().collect();
        let transport = FakeTransport::default()
            .with_get(&anitya_url("htop"), &anitya_body("htop", "3.5.3"))
            .with_get(&anitya_url("vim"), &anitya_body("vim", "9.2.1293"))
            .with_get(
                &anitya_url("rpm-health-no-such-package"),
                &anitya_body("rpm-health-no-such-package", "2.0"),
            )
            .with_post_reply(LATEST_FIXTURE);

        let report = check_upstream(&transport, &refs, Some("f46"));

        assert_eq!(
            report.statuses[0],
            behind("3.5.3", Gap::Minor, Some("3.5.3"), None)
        );
        assert_eq!(
            report.statuses[1],
            behind("9.2.1293", Gap::Minor, Some("9.2.1202"), Some(Gap::Patch))
        );
    }

    #[test]
    fn nothing_is_sent_to_koji_when_nothing_is_behind() {
        let current = installed("current", "1.0");
        let transport = FakeTransport::default()
            .with_get(&anitya_url("current"), &anitya_body("current", "1.0"));

        check_upstream(&transport, &[&current], Some("f46"));

        assert!(transport.post_bodies.borrow().is_empty());
    }

    #[test]
    fn an_anitya_failure_warns_and_marks_everything_not_checked() {
        let package = installed("x", "1.0");
        let transport = FakeTransport::default().with_failing_get(&anitya_url("x"));

        let report = check_upstream(&transport, &[&package], Some("f46"));

        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.statuses[0].label(), "Not checked");
    }

    #[test]
    fn a_koji_failure_warns_and_keeps_the_upstream_result() {
        let package = installed("x", "1.0");
        let transport = FakeTransport::default()
            .with_get(&anitya_url("x"), &anitya_body("x", "2.0"))
            .with_post_reply("not xml-rpc");

        let report = check_upstream(&transport, &[&package], Some("f46"));

        assert_eq!(report.warnings.len(), 1);
        assert_eq!(
            report.statuses[0],
            behind("2.0", Gap::Major, None, Some(Gap::Major))
        );
    }
}
