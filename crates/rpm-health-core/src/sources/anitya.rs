//! release-monitoring.org (Anitya) tracks the latest release of upstream projects
//! and which Fedora package each one maps to.

use std::collections::HashMap;

use reqwest::Url;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use time::Date;
use time::format_description::well_known::Iso8601;

use crate::error::{Error, Result};
use crate::http::Transport;

const PACKAGES_URL: &str = "https://release-monitoring.org/api/v2/packages/";
const PROJECTS_URL: &str = "https://release-monitoring.org/api/v2/projects/";
const VERSIONS_URL: &str = "https://release-monitoring.org/api/v2/versions/";
const DISTRIBUTION: &str = "Fedora";
/// The most the API allows per page.
const ITEMS_PER_PAGE: usize = 250;
/// There's no way to ask about many names at once. Above this many packages,
/// reading the full Fedora list (about 100 pages) beats asking one at a time.
const MAX_SINGLE_LOOKUPS: usize = 10;

#[derive(Deserialize)]
struct Page {
    items: Vec<Item>,
    total_items: usize,
}

#[derive(Deserialize)]
struct Item {
    name: String,
    // `Option` makes a JSON null come through as `None`. Some projects have no
    // stable release.
    stable_version: Option<String>,
    project: String,
    ecosystem: String,
}

/// What release-monitoring.org knows about one Fedora package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamPackage {
    pub stable_version: Option<String>,
    pub project: String,
    pub ecosystem: String,
}

/// One upstream release and the day it was first seen. Old releases can have no
/// date, because they predate release-monitoring.org recording them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRelease {
    pub version: String,
    pub first_seen: Option<Date>,
}

#[derive(Deserialize)]
struct ProjectList {
    items: Vec<Project>,
}

#[derive(Deserialize)]
struct Project {
    id: u64,
    name: String,
    ecosystem: String,
}

#[derive(Deserialize)]
struct VersionList {
    version_details: Vec<VersionDetail>,
}

#[derive(Deserialize)]
struct VersionDetail {
    version: String,
    created_on: Option<String>,
    pre_release: bool,
}

/// Returns what's known about each package Anitya tracks. A package missing from
/// the map isn't tracked.
pub fn fetch_upstream_versions(
    transport: &dyn Transport,
    names: &[String],
) -> Result<HashMap<String, UpstreamPackage>> {
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let items = if names.len() <= MAX_SINGLE_LOOKUPS {
        fetch_one_at_a_time(transport, names)?
    } else {
        fetch_all_pages(transport)?
    };
    Ok(items
        .into_iter()
        .map(|item| {
            let package = UpstreamPackage {
                stable_version: item.stable_version,
                project: item.project,
                ecosystem: item.ecosystem,
            };
            (item.name, package)
        })
        .collect())
}

/// Returns a project's stable releases with the day each was first seen. That can
/// be later than the real release date, but never earlier, so anything relying on
/// it errs towards giving a release more time. Finding these takes two requests,
/// because the release list is keyed by a project ID we have to look up first.
pub fn fetch_releases(
    transport: &dyn Transport,
    project: &str,
    ecosystem: &str,
) -> Result<Vec<UpstreamRelease>> {
    let url = api_url(PROJECTS_URL, &[("name", project), ("ecosystem", ecosystem)])?;
    let list: ProjectList = fetch_json(transport, &url)?;
    let Some(found) = list
        .items
        .iter()
        .find(|item| item.name == project && item.ecosystem == ecosystem)
    else {
        return Ok(Vec::new());
    };

    let url = api_url(VERSIONS_URL, &[("project_id", &found.id.to_string())])?;
    let versions: VersionList = fetch_json(transport, &url)?;
    Ok(versions
        .version_details
        .into_iter()
        .filter(|detail| !detail.pre_release)
        .map(|detail| UpstreamRelease {
            first_seen: detail.created_on.as_deref().and_then(parse_date),
            version: detail.version,
        })
        .collect())
}

/// Timestamps come both with and without a time zone, so only the date part is read.
fn parse_date(timestamp: &str) -> Option<Date> {
    Date::parse(timestamp.get(..10)?, &Iso8601::DATE).ok()
}

fn fetch_one_at_a_time(transport: &dyn Transport, names: &[String]) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    for name in names {
        let url = packages_url(&[("name", name), ("distribution", DISTRIBUTION)])?;
        items.extend(fetch_page(transport, &url)?.items);
    }
    Ok(items)
}

/// The URL of one page of the full Fedora list.
pub fn full_list_url(page: usize) -> Result<String> {
    packages_url(&[
        ("distribution", DISTRIBUTION),
        ("items_per_page", &ITEMS_PER_PAGE.to_string()),
        ("page", &page.to_string()),
    ])
}

fn fetch_all_pages(transport: &dyn Transport) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    let mut page = 1;
    loop {
        let url = full_list_url(page)?;
        let fetched = fetch_page(transport, &url)?;
        // The empty check means a wrong `total_items` can't make this loop forever.
        let finished = fetched.items.is_empty() || page * ITEMS_PER_PAGE >= fetched.total_items;
        items.extend(fetched.items);
        if finished {
            return Ok(items);
        }
        page += 1;
    }
}

fn fetch_page(transport: &dyn Transport, url: &str) -> Result<Page> {
    fetch_json(transport, url)
}

/// Generic over `T`, so one function can parse any of the response types above.
/// `DeserializeOwned` means "can be built from JSON without borrowing from it".
fn fetch_json<T: DeserializeOwned>(transport: &dyn Transport, url: &str) -> Result<T> {
    let body = transport.get(url)?.into_success_body(url)?;
    serde_json::from_str(&body).map_err(|e| parse_error(e.to_string()))
}

/// Url handles the encoding, which matters for names like `gcc-c++`, because a
/// plain `+` in a query string means a space.
fn packages_url(params: &[(&str, &str)]) -> Result<String> {
    api_url(PACKAGES_URL, params)
}

fn api_url(base: &str, params: &[(&str, &str)]) -> Result<String> {
    let url = Url::parse_with_params(base, params).map_err(|e| parse_error(e.to_string()))?;
    Ok(url.to_string())
}

fn parse_error(message: String) -> Error {
    Error::Parse {
        what: "release-monitoring.org data",
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::FakeTransport;

    /// A real response for htop.
    const FIXTURE: &str = include_str!("../../tests/fixtures/anitya_package.json");

    fn page_url(page: usize) -> String {
        full_list_url(page).unwrap()
    }

    fn single_url(name: &str) -> String {
        packages_url(&[("name", name), ("distribution", DISTRIBUTION)]).unwrap()
    }

    fn page_body(names: &[&str], total_items: usize) -> String {
        let items: Vec<String> = names
            .iter()
            .map(|name| format!(
                    "{{\"name\": \"{name}\", \"stable_version\": \"1.0\", \"project\": \"{name}\", \"ecosystem\": \"pypi\"}}"
                ))
            .collect();
        format!(
            "{{\"items\": [{}], \"total_items\": {total_items}}}",
            items.join(",")
        )
    }

    fn many_names() -> Vec<String> {
        (0..=MAX_SINGLE_LOOKUPS).map(|i| format!("p{i}")).collect()
    }

    #[test]
    fn reads_a_real_response() {
        let transport = FakeTransport::default().with_get(&single_url("htop"), FIXTURE);
        let versions = fetch_upstream_versions(&transport, &["htop".to_string()]).unwrap();
        assert_eq!(versions["htop"].stable_version.as_deref(), Some("3.5.3"));
        assert_eq!(versions["htop"].project, "htop");
    }

    #[test]
    fn a_tracked_project_without_a_stable_release_is_none() {
        let body = "{\"items\": [{\"name\": \"x\", \"stable_version\": null, \"project\": \"x\", \"ecosystem\": \"pypi\"}], \"total_items\": 1}";
        let transport = FakeTransport::default().with_get(&single_url("x"), body);
        let versions = fetch_upstream_versions(&transport, &["x".to_string()]).unwrap();
        assert_eq!(versions["x"].stable_version, None);
    }

    #[test]
    fn untracked_packages_are_absent() {
        let body = page_body(&[], 0);
        let transport = FakeTransport::default().with_get(&single_url("mine"), &body);
        let versions = fetch_upstream_versions(&transport, &["mine".to_string()]).unwrap();
        assert!(versions.is_empty());
    }

    #[test]
    fn plus_signs_in_names_are_encoded() {
        assert!(single_url("gcc-c++").contains("name=gcc-c%2B%2B"));
    }

    #[test]
    fn many_names_read_every_page_of_the_full_list() {
        let transport = FakeTransport::default()
            .with_get(&page_url(1), &page_body(&["a"], 251))
            .with_get(&page_url(2), &page_body(&["b"], 251));
        let versions = fetch_upstream_versions(&transport, &many_names()).unwrap();
        assert_eq!(versions.len(), 2);
        assert!(versions.contains_key("b"));
    }

    #[test]
    fn an_empty_page_stops_the_loop_even_if_the_total_is_wrong() {
        let transport = FakeTransport::default()
            .with_get(&page_url(1), &page_body(&["a"], 99999))
            .with_get(&page_url(2), &page_body(&[], 99999));
        let versions = fetch_upstream_versions(&transport, &many_names()).unwrap();
        assert_eq!(versions.len(), 1);
    }

    #[test]
    fn a_failed_page_fails_the_whole_fetch() {
        let transport = FakeTransport::default().with_get(&page_url(1), &page_body(&["a"], 251));
        assert!(matches!(
            fetch_upstream_versions(&transport, &many_names()),
            Err(Error::BadStatus { status: 404, .. })
        ));
    }

    #[test]
    fn no_names_means_no_requests() {
        let transport = FakeTransport::default();
        assert!(fetch_upstream_versions(&transport, &[]).unwrap().is_empty());
    }

    /// Real responses for filelock. The version list is cut down to four real
    /// entries to keep the fixture readable, including one with no date.
    const PROJECT_FIXTURE: &str = include_str!("../../tests/fixtures/anitya_project.json");
    const VERSIONS_FIXTURE: &str = include_str!("../../tests/fixtures/anitya_versions.json");

    fn project_url(name: &str, ecosystem: &str) -> String {
        api_url(PROJECTS_URL, &[("name", name), ("ecosystem", ecosystem)]).unwrap()
    }

    fn versions_url(id: u64) -> String {
        api_url(VERSIONS_URL, &[("project_id", &id.to_string())]).unwrap()
    }

    #[test]
    fn reads_release_dates_from_real_responses() {
        let transport = FakeTransport::default()
            .with_get(&project_url("filelock", "pypi"), PROJECT_FIXTURE)
            .with_get(&versions_url(11739), VERSIONS_FIXTURE);

        let releases = fetch_releases(&transport, "filelock", "pypi").unwrap();

        let release = releases.iter().find(|r| r.version == "4.2.9").unwrap();
        assert_eq!(release.first_seen.unwrap().to_string(), "2026-11-04");
        assert_eq!(releases.len(), 4);
        assert_eq!(
            releases.iter().filter(|r| r.first_seen.is_none()).count(),
            1
        );
    }

    #[test]
    fn an_unknown_project_has_no_releases_and_makes_no_second_request() {
        let body = "{\"items\": []}";
        let transport = FakeTransport::default().with_get(&project_url("x", "pypi"), body);

        assert!(fetch_releases(&transport, "x", "pypi").unwrap().is_empty());
        assert_eq!(transport.get_urls.borrow().len(), 1);
    }

    #[test]
    fn a_similarly_named_project_is_not_used() {
        let body = "{\"items\": [{\"id\": 1, \"name\": \"x\", \"ecosystem\": \"cran\"}]}";
        let transport = FakeTransport::default().with_get(&project_url("x", "pypi"), body);
        assert!(fetch_releases(&transport, "x", "pypi").unwrap().is_empty());
    }

    #[test]
    fn prereleases_are_left_out_and_unusable_dates_become_none() {
        let project = "{\"items\": [{\"id\": 1, \"name\": \"x\", \"ecosystem\": \"pypi\"}]}";
        let versions = "{\"version_details\": [\
            {\"version\": \"2.0rc1\", \"created_on\": \"2026-01-01T00:00:00Z\", \"pre_release\": true},\
            {\"version\": \"2.0\", \"created_on\": \"soon\", \"pre_release\": false},\
            {\"version\": \"1.5\", \"created_on\": null, \"pre_release\": false},\
            {\"version\": \"1.0\", \"created_on\": \"2025-01-01T10:00:00Z\", \"pre_release\": false}]}";
        let transport = FakeTransport::default()
            .with_get(&project_url("x", "pypi"), project)
            .with_get(&versions_url(1), versions);

        let releases = fetch_releases(&transport, "x", "pypi").unwrap();

        let versions: Vec<&str> = releases.iter().map(|r| r.version.as_str()).collect();
        assert_eq!(versions, ["2.0", "1.5", "1.0"]);
        assert_eq!(releases[0].first_seen, None);
        assert_eq!(releases[1].first_seen, None);
        assert_eq!(releases[2].first_seen.unwrap().to_string(), "2025-01-01");
    }
}
