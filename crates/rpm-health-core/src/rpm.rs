use std::collections::BTreeMap;
use std::process::Command;

use crate::error::{Error, Result};
use crate::model::InstalledPackage;

const QUERY_FORMAT: &str = "%{NAME}\t%{VERSION}-%{RELEASE}\t%{SOURCERPM}\t%{VENDOR}\n";
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

    for line in output.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let [name, version, source_rpm, vendor] = fields[..] else {
            return Err(Error::Rpm(format!("unexpected rpm output: {line:?}")));
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
            });
        entry.binaries.push(name.to_string());
        entry.versions.push(version.to_string());
        entry.from_fedora |= vendor == FEDORA_VENDOR;
    }

    let mut packages: Vec<InstalledPackage> = by_source.into_values().collect();
    for package in &mut packages {
        package.binaries.sort();
        package.binaries.dedup();
        package.versions.sort();
        package.versions.dedup();
    }
    Ok(packages)
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

    const SAMPLE: &str = "\
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
        let packages = parse_rpm_output(SAMPLE).unwrap();
        assert_eq!(find(&packages, "aalib").binaries, ["aalib-libs"]);
    }

    #[test]
    fn groups_binaries_built_from_the_same_source_package() {
        let packages = parse_rpm_output(SAMPLE).unwrap();
        let fonts = find(&packages, "abattis-cantarell-fonts");
        assert_eq!(
            fonts.binaries,
            ["abattis-cantarell-fonts", "abattis-cantarell-vf-fonts"]
        );
        assert_eq!(fonts.versions, ["0.301-18.fc45"]);
    }

    #[test]
    fn keeps_every_installed_version_of_a_package() {
        let packages = parse_rpm_output(SAMPLE).unwrap();
        let kernel = find(&packages, "kernel");
        assert_eq!(kernel.binaries, ["kernel"]);
        assert_eq!(kernel.versions, ["7.2.13-200.fc44", "7.2.15-300.fc45"]);
    }

    #[test]
    fn skips_signing_keys() {
        let packages = parse_rpm_output(SAMPLE).unwrap();
        assert!(packages.iter().all(|p| p.source_name != "gpg-pubkey"));
    }

    #[test]
    fn falls_back_to_the_binary_name_without_a_source_rpm() {
        let packages = parse_rpm_output(SAMPLE).unwrap();
        assert!(!find(&packages, "dbeaver-ce").from_fedora);
    }

    #[test]
    fn copr_vendor_does_not_count_as_fedora() {
        let packages = parse_rpm_output(SAMPLE).unwrap();
        assert!(!find(&packages, "some-tool").from_fedora);
        assert!(find(&packages, "kernel").from_fedora);
    }

    #[test]
    fn rejects_lines_with_the_wrong_number_of_fields() {
        assert!(matches!(
            parse_rpm_output("not\tenough\tfields"),
            Err(Error::Rpm(_))
        ));
    }
}
