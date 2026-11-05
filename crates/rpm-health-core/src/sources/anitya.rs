//! release-monitoring.org (Anitya) tracks the latest release of upstream projects
//! and which Fedora package each one maps to.

use std::collections::HashMap;

use reqwest::Url;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::http::Transport;

const PACKAGES_URL: &str = "https://release-monitoring.org/api/v2/packages/";
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
}

/// Returns upstream's stable version for each package Anitya knows about. A
/// package missing from the map isn't tracked. One with `None` is tracked but
/// has no stable release.
pub fn fetch_upstream_versions(
    transport: &dyn Transport,
    names: &[String],
) -> Result<HashMap<String, Option<String>>> {
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
        .map(|item| (item.name, item.stable_version))
        .collect())
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
    let body = transport.get(url)?.into_success_body(url)?;
    serde_json::from_str(&body).map_err(|e| parse_error(e.to_string()))
}

/// Url handles the encoding, which matters for names like `gcc-c++`, because a
/// plain `+` in a query string means a space.
fn packages_url(params: &[(&str, &str)]) -> Result<String> {
    let url =
        Url::parse_with_params(PACKAGES_URL, params).map_err(|e| parse_error(e.to_string()))?;
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
            .map(|name| format!("{{\"name\": \"{name}\", \"stable_version\": \"1.0\"}}"))
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
        assert_eq!(versions["htop"].as_deref(), Some("3.5.3"));
    }

    #[test]
    fn a_tracked_project_without_a_stable_release_is_none() {
        let body = "{\"items\": [{\"name\": \"x\", \"stable_version\": null}], \"total_items\": 1}";
        let transport = FakeTransport::default().with_get(&single_url("x"), body);
        let versions = fetch_upstream_versions(&transport, &["x".to_string()]).unwrap();
        assert_eq!(versions["x"], None);
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
}
