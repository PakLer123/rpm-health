use std::collections::BTreeMap;
use std::process::Command;

use crate::dates::date_from_unix;
use crate::error::{Error, Result};
use crate::model::{ChangelogEntry, InstalledPackage};

// Changelog text can contain newlines and tabs, so records and entries end with
// ASCII control characters that real text doesn't use. The `[...]` part repeats
// once per changelog entry.
const RECORD_END: char = '\x1d';
const ENTRY_END: char = '\x1e';
const FIELD_END: char = '\x1f';
const QUERY_FORMAT: &str = "%{NAME}\t%{VERSION}-%{RELEASE}\t%{SOURCERPM}\t%{VENDOR}\t\
    [%{CHANGELOGTIME}\x1f%{CHANGELOGNAME}\x1f%{CHANGELOGTEXT}\x1e]\x1d";
/// Compared exactly, because "Fedora Copr - user someone" is not Fedora.
const FEDORA_VENDOR: &str = "Fedora Project";

/// Asks `rpm` for everything installed. This only ever reads.
pub fn list_installed() -> Result<Vec<InstalledPackage>> {
    let output = Command::new("rpm")
        .args(["-qa", "--queryformat", QUERY_FORMAT])
        .output()
        // `map_err` converts the error type, and `?` returns early if this is
        // an `Err`. Otherwise `?` unwraps the `Ok` value and carries on.
        .map_err(|e| Error::Rpm(format!("could not run rpm: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Rpm(format!("rpm failed: {}", stderr.trim())));
    }
    parse_rpm_output(&String::from_utf8_lossy(&output.stdout))
}

/// Turns `rpm` output into one entry per source package, sorted by name.
///
/// This is separate from `list_installed` so tests can feed it sample text.
/// The parameter `&str` is a borrowed view of text: this function reads it
/// without taking ownership or making a copy.
pub fn parse_rpm_output(output: &str) -> Result<Vec<InstalledPackage>> {
    // A BTreeMap keeps its keys sorted, which gives us a stable output order.
    let mut by_source: BTreeMap<String, InstalledPackage> = BTreeMap::new();

    for record in output.split(RECORD_END).filter(|r| !r.trim().is_empty()) {
        // `splitn` stops after five fields, so tabs in the changelog are safe.
        let fields: Vec<&str> = record.splitn(5, '\t').collect();
        let [name, version, source_rpm, vendor, changelog] = fields[..] else {
            return Err(Error::Rpm(format!("unexpected rpm output: {record:?}")));
        };

        // These are signing keys stored in the RPM database, not software.
        if name == "gpg-pubkey" {
            continue;
        }

        let source_name = source_name(name, source_rpm);
        let entry = by_source
            .entry(source_name.to_string())
            .or_insert_with(|| InstalledPackage {
                source_name: source_name.to_string(),
                binaries: Vec::new(),
                versions: Vec::new(),
                from_fedora: false,
                changelog: Vec::new(),
            });
        entry.binaries.push(name.to_string());
        entry.versions.push(version.to_string());
        if vendor == FEDORA_VENDOR {
            entry.from_fedora = true;
            entry.changelog.extend(parse_changelog(changelog));
        }
    }

    let mut packages: Vec<InstalledPackage> = by_source.into_values().collect();
    for package in &mut packages {
        package.binaries.sort();
        package.binaries.dedup();
        package.versions.sort();
        package.versions.dedup();
        // Every RPM from a source package repeats the same changelog.
        package.changelog.sort();
        package.changelog.dedup();
        package.changelog.reverse();
    }
    Ok(packages)
}

/// Entries that can't be read are skipped, since one bad entry shouldn't stop a
/// scan. Packages without a changelog simply give an empty list.
fn parse_changelog(text: &str) -> Vec<ChangelogEntry> {
    text.split(ENTRY_END)
        .filter_map(|entry| {
            let mut fields = entry.split(FIELD_END);
            let seconds: i64 = fields.next()?.parse().ok()?;
            Some(ChangelogEntry {
                date: date_from_unix(seconds)?,
                author: fields.next()?.to_string(),
                text: fields.next()?.to_string(),
            })
        })
        .collect()
}

/// Gets `aalib` from `aalib-1.4.0-0.59.rc5.fc45.src.rpm`.
///
/// Names can contain hyphens but versions and releases can't, so we split from
/// the right. Some third-party RPMs have a source RPM of `(none)`, and for
/// those the binary name is the best we can do.
fn source_name<'a>(name: &'a str, source_rpm: &'a str) -> &'a str {
    source_rpm.rsplitn(3, '-').nth(2).unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Readable sample lines, each turned into a record with no changelog.
    fn sample() -> String {
        SAMPLE_LINES.replace('\n', "\t\x1d")
    }

    const SAMPLE_LINES: &str = "\
aalib-libs\t1.4.0-0.59.rc5.fc45\taalib-1.4.0-0.59.rc5.fc45.src.rpm\tFedora Project
abattis-cantarell-fonts\t0.301-18.fc45\tabattis-cantarell-fonts-0.301-18.fc45.src.rpm\tFedora Project
abattis-cantarell-vf-fonts\t0.301-18.fc45\tabattis-cantarell-fonts-0.301-18.fc45.src.rpm\tFedora Project
dbeaver-ce\t26.2.1-stable\t(none)\tDBeaver Corp
gpg-pubkey\tabc-def\t(none)\t(none)
kernel\t7.2.13-200.fc44\tkernel-7.2.13-200.fc44.src.rpm\tFedora Project
kernel\t7.2.15-300.fc45\tkernel-7.2.15-300.fc45.src.rpm\tFedora Project
some-tool\t1.0-1.fc45\tsome-tool-1.0-1.fc45.src.rpm\tFedora Copr - user someone
";

    fn find<'a>(packages: &'a [InstalledPackage], name: &str) -> &'a InstalledPackage {
        packages.iter().find(|p| p.source_name == name).unwrap()
    }

    #[test]
    fn maps_a_binary_to_a_differently_named_source_package() {
        let packages = parse_rpm_output(&sample()).unwrap();
        assert_eq!(find(&packages, "aalib").binaries, ["aalib-libs"]);
    }

    #[test]
    fn groups_binaries_built_from_the_same_source_package() {
        let packages = parse_rpm_output(&sample()).unwrap();
        let fonts = find(&packages, "abattis-cantarell-fonts");
        assert_eq!(
            fonts.binaries,
            ["abattis-cantarell-fonts", "abattis-cantarell-vf-fonts"]
        );
        assert_eq!(fonts.versions, ["0.301-18.fc45"]);
    }

    #[test]
    fn keeps_every_installed_version_of_a_package() {
        let packages = parse_rpm_output(&sample()).unwrap();
        let kernel = find(&packages, "kernel");
        assert_eq!(kernel.binaries, ["kernel"]);
        assert_eq!(kernel.versions, ["7.2.13-200.fc44", "7.2.15-300.fc45"]);
    }

    #[test]
    fn skips_signing_keys() {
        let packages = parse_rpm_output(&sample()).unwrap();
        assert!(packages.iter().all(|p| p.source_name != "gpg-pubkey"));
    }

    #[test]
    fn falls_back_to_the_binary_name_without_a_source_rpm() {
        let packages = parse_rpm_output(&sample()).unwrap();
        assert!(!find(&packages, "dbeaver-ce").from_fedora);
    }

    #[test]
    fn copr_vendor_does_not_count_as_fedora() {
        let packages = parse_rpm_output(&sample()).unwrap();
        assert!(!find(&packages, "some-tool").from_fedora);
        assert!(find(&packages, "kernel").from_fedora);
    }

    #[test]
    fn rejects_lines_with_the_wrong_number_of_fields() {
        assert!(matches!(
            parse_rpm_output("not\tenough\tfields\x1d"),
            Err(Error::Rpm(_))
        ));
    }

    /// Two RPMs from one source package, like `rpm` really reports them.
    const WITH_CHANGELOG: &str = "\
a\t1-1.fc45\tsrc-1-1.fc45.src.rpm\tFedora Project\t\
1784116800\x1fReleng <releng@fedoraproject.org>\x1f- Rebuilt\twith a tab\nand a newline\x1e\
1700000000\x1fAlice <alice@example.com>\x1f- Update\x1e\x1d\
b\t1-1.fc45\tsrc-1-1.fc45.src.rpm\tFedora Project\t\
1784116800\x1fReleng <releng@fedoraproject.org>\x1f- Rebuilt\twith a tab\nand a newline\x1e\
1700000000\x1fAlice <alice@example.com>\x1f- Update\x1e\x1d";

    #[test]
    fn reads_the_changelog_newest_first_without_repeats() {
        let packages = parse_rpm_output(WITH_CHANGELOG).unwrap();
        let changelog = &packages[0].changelog;
        assert_eq!(changelog.len(), 2);
        assert_eq!(changelog[0].date.to_string(), "2026-07-15");
        assert_eq!(changelog[0].text, "- Rebuilt\twith a tab\nand a newline");
        assert_eq!(changelog[1].author, "Alice <alice@example.com>");
    }

    #[test]
    fn ignores_changelogs_from_third_party_rpms_of_the_same_source() {
        let output = "\
fedora-part\t1-1.fc45\tffmpeg-1-1.fc45.src.rpm\tFedora Project\t1700000000\x1fA\x1f- Old\x1e\x1d\
fusion-part\t1-1.fc45\tffmpeg-1-1.fc45.src.rpm\tRPM Fusion\t1784116800\x1fB\x1f- New\x1e\x1d";
        let packages = parse_rpm_output(output).unwrap();
        assert!(packages[0].from_fedora);
        assert_eq!(packages[0].changelog.len(), 1);
        assert_eq!(packages[0].changelog[0].text, "- Old");
    }

    #[test]
    fn skips_changelog_entries_it_cannot_read() {
        let entries = parse_changelog("not-a-number\x1fA\x1f- x\x1e1700000000\x1fB\x1f- y\x1e");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "- y");
    }
}
