//! Works out which Fedora release this system runs.

use std::fs;

use crate::error::{Error, Result};

const OS_RELEASE_PATH: &str = "/etc/os-release";

/// Returns this system's Koji tag, such as `f45`.
pub fn current_tag() -> Result<String> {
    let text = fs::read_to_string(OS_RELEASE_PATH)
        .map_err(|e| Error::OsRelease(format!("could not read {OS_RELEASE_PATH}: {e}")))?;
    parse_tag(&text)
}

fn parse_tag(os_release: &str) -> Result<String> {
    let id = field(os_release, "ID");
    if id != Some("fedora") {
        return Err(Error::OsRelease(format!(
            "rpm-health only supports Fedora, but this system's ID is {}",
            id.unwrap_or("not set")
        )));
    }
    let version = field(os_release, "VERSION_ID")
        .ok_or_else(|| Error::OsRelease("VERSION_ID is not set".to_string()))?;
    Ok(format!("f{version}"))
}

/// Finds `KEY=value` or `KEY="value"` in os-release text.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (found_key, value) = line.split_once('=')?;
        (found_key == key).then(|| value.trim_matches('"'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_tag_from_the_version() {
        let text = "NAME=\"Fedora Linux\"\nID=fedora\nVERSION_ID=45\n";
        assert_eq!(parse_tag(text).unwrap(), "f45");
    }

    #[test]
    fn id_like_does_not_count_as_fedora() {
        let text = "ID=\"rhel\"\nID_LIKE=\"fedora\"\nVERSION_ID=\"10\"\n";
        assert!(matches!(parse_tag(text), Err(Error::OsRelease(_))));
    }

    #[test]
    fn missing_version_is_an_error() {
        assert!(matches!(parse_tag("ID=fedora\n"), Err(Error::OsRelease(_))));
    }
}
