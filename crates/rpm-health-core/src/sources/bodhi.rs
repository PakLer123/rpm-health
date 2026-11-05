//! Bodhi, Fedora's update system. For now we only ask it which release is
//! Rawhide, the development version that becomes the next Fedora.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::http::Transport;

pub const PENDING_RELEASES_URL: &str = "https://bodhi.fedoraproject.org/releases/?state=pending";
const RAWHIDE_BRANCH: &str = "rawhide";

#[derive(Deserialize)]
struct ReleaseList {
    releases: Vec<Release>,
}

#[derive(Deserialize)]
struct Release {
    branch: String,
    dist_tag: String,
}

/// Returns Rawhide's Koji tag, such as `f46`.
pub fn rawhide_tag(transport: &dyn Transport) -> Result<String> {
    let body = transport
        .get(PENDING_RELEASES_URL)?
        .into_success_body(PENDING_RELEASES_URL)?;
    let list: ReleaseList = serde_json::from_str(&body).map_err(|e| parse_error(e.to_string()))?;
    list.releases
        .into_iter()
        .find(|release| release.branch == RAWHIDE_BRANCH)
        .map(|release| release.dist_tag)
        .ok_or_else(|| parse_error("no pending release is on the rawhide branch".to_string()))
}

fn parse_error(message: String) -> Error {
    Error::Parse {
        what: "Bodhi release list",
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::FakeTransport;

    const FIXTURE: &str = include_str!("../../tests/fixtures/bodhi_pending.json");

    #[test]
    fn finds_rawhide_among_pending_releases() {
        let transport = FakeTransport::default().with_get(PENDING_RELEASES_URL, FIXTURE);
        assert_eq!(rawhide_tag(&transport).unwrap(), "f46");
    }

    #[test]
    fn missing_rawhide_is_an_error() {
        let transport =
            FakeTransport::default().with_get(PENDING_RELEASES_URL, "{\"releases\": []}");
        assert!(matches!(rawhide_tag(&transport), Err(Error::Parse { .. })));
    }
}
