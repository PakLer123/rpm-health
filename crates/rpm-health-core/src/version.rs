//! Compares version strings, and works out how far behind upstream a package is.
//!
//! Upstream projects format versions however they like, and release-monitoring.org
//! sometimes has wrong or outdated data. So a comparison is allowed to answer
//! "these can't be compared fairly", which is better than raising a false alarm.

use std::cmp::Ordering;

use serde::Serialize;

/// How big a difference between two versions is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Gap {
    Major,
    Minor,
    Patch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comparison {
    Same,
    Behind(Gap),
    Ahead,
    NotComparable(&'static str),
}

/// One run of digits or letters from a version string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Segment<'a> {
    Number(u64),
    Text(&'a str),
}

/// Splits `10.6p2` into `10`, `6`, `p`, `2`. Other characters only separate.
fn segments(version: &str) -> Vec<Segment<'_>> {
    let mut found = Vec::new();
    let mut rest = version;
    while let Some(start) = rest.find(|c: char| c.is_ascii_alphanumeric()) {
        rest = &rest[start..];
        let is_digit = rest.starts_with(|c: char| c.is_ascii_digit());
        let end = rest
            .find(|c: char| {
                if is_digit {
                    !c.is_ascii_digit()
                } else {
                    !c.is_ascii_alphabetic()
                }
            })
            .unwrap_or(rest.len());
        let (run, remainder) = rest.split_at(end);
        // A number too long for u64 (like a git hash) can be compared as text.
        found.push(match run.parse() {
            Ok(number) if is_digit => Segment::Number(number),
            _ => Segment::Text(run),
        });
        rest = remainder;
    }
    found
}

/// Orders two versions, following the same idea as RPM: numbers compare as
/// numbers, so `1.10` is newer than `1.9`, and a number beats text. Missing
/// zeros don't count, so `3.5` equals `3.5.0`.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let (a, b) = (segments(a), segments(b));
    for i in 0..a.len().max(b.len()) {
        let order = compare_segments(a.get(i).copied(), b.get(i).copied());
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
}

fn compare_segments(a: Option<Segment>, b: Option<Segment>) -> Ordering {
    use Segment::{Number, Text};
    match (a, b) {
        (Some(Number(x)), Some(Number(y))) => x.cmp(&y),
        (Some(Text(x)), Some(Text(y))) => x.cmp(y),
        (Some(Number(_)), Some(Text(_))) => Ordering::Greater,
        (Some(Text(_)), Some(Number(_))) => Ordering::Less,
        (Some(Number(0)), None) | (None, Some(Number(0))) | (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
    }
}

/// Compares an installed version with what upstream has released.
pub fn compare_to_upstream(package: &str, installed: &str, upstream: &str) -> Comparison {
    let installed = base_version(installed);
    let upstream = normalize_upstream(package, upstream);

    if !is_plain_version(installed) {
        return Comparison::NotComparable("the installed version has an unusual format");
    }
    if !is_plain_version(&upstream) {
        return Comparison::NotComparable("upstream's version has an unusual format");
    }
    if uses_different_numbering(installed, &upstream) {
        return Comparison::NotComparable("the versions use different numbering schemes");
    }

    match compare_versions(installed, &upstream) {
        Ordering::Equal => Comparison::Same,
        Ordering::Greater => Comparison::Ahead,
        Ordering::Less => Comparison::Behind(gap_between(installed, &upstream)),
    }
}

/// Drops the parts RPM adds to mark snapshots and pre-releases, so
/// `1.5^git20250123` compares as `1.5`.
fn base_version(version: &str) -> &str {
    version.split(['^', '~', '+']).next().unwrap_or(version)
}

/// Undoes common upstream spellings: `v1.2`, `1_4_4`, and `usermode-1.114`.
fn normalize_upstream(package: &str, upstream: &str) -> String {
    let without_name = upstream
        .strip_prefix(package)
        .and_then(|rest| rest.strip_prefix('-'))
        .unwrap_or(upstream);
    let without_v = without_name
        .strip_prefix(['v', 'V'])
        .unwrap_or(without_name);
    base_version(without_v).replace('_', ".")
}

/// Accepts dotted numbers with an optional single-letter or `p2`-style suffix.
/// That leaves out betas like `3.0.0b3`, whatever the data source calls them.
fn is_plain_version(version: &str) -> bool {
    let numeric_end = version
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(version.len());
    let (numbers, suffix) = version.split_at(numeric_end);

    let numbers_ok = numbers
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    let suffix_ok = match suffix.as_bytes() {
        [] => true,
        [letter] => letter.is_ascii_lowercase(),
        [b'p', digits @ ..] => digits.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    numbers_ok && suffix_ok
}

/// Catches pairs like `2.1` and `20260925`, where one is a date.
fn uses_different_numbering(a: &str, b: &str) -> bool {
    let first_length = |version: &str| {
        version
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .map_or(0, str::len)
    };
    first_length(a).abs_diff(first_length(b)) > 1
}

fn gap_between(a: &str, b: &str) -> Gap {
    let (a_segments, b_segments) = (segments(a), segments(b));
    let first_difference = (0..a_segments.len().max(b_segments.len()))
        .find(|&i| {
            compare_segments(a_segments.get(i).copied(), b_segments.get(i).copied())
                != Ordering::Equal
        })
        .unwrap_or(0);

    match first_difference {
        // In a single-number scheme like `62` to `63`, every release changes
        // the first number, so that doesn't say much about the size of the gap.
        0 if a_segments.len() > 1 && b_segments.len() > 1 => Gap::Major,
        0 | 1 => Gap::Minor,
        _ => Gap::Patch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compare(package: &str, installed: &str, upstream: &str) -> Comparison {
        compare_to_upstream(package, installed, upstream)
    }

    #[test]
    fn numbers_compare_as_numbers_not_text() {
        assert_eq!(compare_versions("1.9", "1.10"), Ordering::Less);
        assert_eq!(compare_versions("019", "20"), Ordering::Less);
    }

    #[test]
    fn missing_zeros_do_not_count() {
        assert_eq!(compare_versions("3.5", "3.5.0"), Ordering::Equal);
        assert_eq!(compare_versions("3.5.1", "3.5"), Ordering::Greater);
    }

    #[test]
    fn suffixes_sort_after_the_plain_version() {
        assert_eq!(compare_versions("1.9.17", "1.9.17p2"), Ordering::Less);
        assert_eq!(compare_versions("2026e", "2026f"), Ordering::Less);
    }

    #[test]
    fn huge_numbers_do_not_break_comparison() {
        let long = "99999999999999999999999999";
        assert_eq!(compare_versions(long, long), Ordering::Equal);
    }

    #[test]
    fn classifies_real_gaps() {
        assert_eq!(compare("htop", "3.5.3", "3.5.3"), Comparison::Same);
        assert_eq!(
            compare("c", "1.34.8", "1.34.9"),
            Comparison::Behind(Gap::Patch)
        );
        assert_eq!(
            compare("nano", "9.2", "9.3"),
            Comparison::Behind(Gap::Minor)
        );
        assert_eq!(
            compare("p", "2.5.1", "3.0.10"),
            Comparison::Behind(Gap::Major)
        );
    }

    #[test]
    fn a_new_single_number_release_is_not_a_major_gap() {
        assert_eq!(compare("inih", "62", "63"), Comparison::Behind(Gap::Minor));
        assert_eq!(
            compare("usbutils", "019", "020"),
            Comparison::Behind(Gap::Minor)
        );
    }

    #[test]
    fn suffix_only_differences_are_small() {
        assert_eq!(
            compare("sudo", "1.9.17", "1.9.17p2"),
            Comparison::Behind(Gap::Patch)
        );
        assert_eq!(
            compare("tzdata", "2026e", "2026f"),
            Comparison::Behind(Gap::Minor)
        );
        assert_eq!(
            compare("o", "10.5p1", "10.6p2"),
            Comparison::Behind(Gap::Minor)
        );
    }

    /// Anitya really does have kernel 6.19 while Fedora ships 7.2.
    #[test]
    fn installed_newer_than_upstream_is_ahead() {
        assert_eq!(compare("kernel", "7.2.15", "6.19.14"), Comparison::Ahead);
    }

    #[test]
    fn snapshots_compare_by_their_base_version() {
        assert_eq!(
            compare("c", "1.5^git20250123.d79eacf", "1.5"),
            Comparison::Same
        );
        assert_eq!(
            compare("b", "5.87+1.git8750129efca8", "5.87"),
            Comparison::Same
        );
    }

    #[test]
    fn common_upstream_spellings_are_understood() {
        assert_eq!(
            compare("usermode", "1.114", "usermode-1.114"),
            Comparison::Same
        );
        assert_eq!(compare("inkscape", "1.4.4", "1_4_4"), Comparison::Same);
        assert_eq!(compare("a", "1.2", "v1.2"), Comparison::Same);
    }

    #[test]
    fn prereleases_and_odd_formats_are_not_compared() {
        for upstream in [
            "3.0.0b3",
            "3.48.TRIAL1",
            "5.6.6.git",
            "1.2.3-alt1",
            "2.1.ROLLING",
        ] {
            assert!(
                matches!(compare("x", "1.0", upstream), Comparison::NotComparable(_)),
                "{upstream} should not be comparable"
            );
        }
        assert!(matches!(
            compare("python-dateutil", "2.9.0.post0", "2.9.0"),
            Comparison::NotComparable(_)
        ));
    }

    #[test]
    fn dates_and_ordinary_versions_are_not_compared() {
        assert!(matches!(
            compare("microcode_ctl", "2.1", "20260925"),
            Comparison::NotComparable(_)
        ));
    }

    #[test]
    fn empty_upstream_is_not_comparable() {
        assert!(matches!(
            compare("x", "1.0", ""),
            Comparison::NotComparable(_)
        ));
    }
}
