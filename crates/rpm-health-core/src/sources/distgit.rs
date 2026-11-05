//! Fedora's package git repositories. Retiring a package replaces its files
//! with a `dead.package` file that explains why.

use crate::error::Result;
use crate::http::Transport;

pub fn dead_package_url(package: &str, branch: &str) -> String {
    format!("https://src.fedoraproject.org/rpms/{package}/raw/{branch}/f/dead.package")
}

/// Returns the retirement note, or `None` if the branch has no `dead.package`.
pub fn dead_package_note(
    transport: &dyn Transport,
    package: &str,
    branch: &str,
) -> Result<Option<String>> {
    let url = dead_package_url(package, branch);
    let response = transport.get(&url)?;
    if response.status == 404 {
        return Ok(None);
    }
    let body = response.into_success_body(&url)?;
    Ok(Some(body.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::http::fake::FakeTransport;

    const FIXTURE: &str = include_str!("../../tests/fixtures/dead_linux_atm.txt");

    #[test]
    fn returns_the_note_without_surrounding_whitespace() {
        let url = dead_package_url("linux-atm", "f45");
        let transport = FakeTransport::default().with_get(&url, FIXTURE);
        let note = dead_package_note(&transport, "linux-atm", "f45").unwrap();
        assert_eq!(
            note.as_deref(),
            Some("Dead since kernel 7.2 due to missing UAPI headers, see rhbz#2504303")
        );
    }

    #[test]
    fn a_404_means_no_note() {
        let transport = FakeTransport::default();
        assert_eq!(dead_package_note(&transport, "htop", "f45").unwrap(), None);
    }

    #[test]
    fn network_failures_are_errors() {
        let url = dead_package_url("htop", "f45");
        let transport = FakeTransport::default().with_failing_get(&url);
        assert!(matches!(
            dead_package_note(&transport, "htop", "f45"),
            Err(Error::Http { .. })
        ));
    }
}
