use std::process::ExitCode;

use clap::{Parser, Subcommand};
use comfy_table::{ContentArrangement, Table};
use rpm_health_core::error::Result;
use rpm_health_core::http::HttpClient;
use rpm_health_core::model::{ActivityStatus, InstalledPackage, MaintainerStatus, PackageStatus};
use rpm_health_core::signals::activity::{ActivityReport, PackageActivity, check_activity};
use rpm_health_core::signals::maintainer::{MaintainerReport, check_maintainers};
use rpm_health_core::{dates, release, rpm};

const MAX_RPMS_SHOWN: usize = 3;

// `derive(Parser)` generates the argument parsing and `--help` text from this
// struct. `///` doc comments become the help text, so explanations for readers
// of the code, like this one, use `//`.

/// Finds installed Fedora packages that are at risk of dying or going stale.
#[derive(Parser)]
#[command(name = "rpm-health", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check all installed packages and list the ones that need attention
    Scan,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Scan => scan(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn scan() -> Result<()> {
    let current_tag = release::current_tag()?;
    let packages = rpm::list_installed()?;
    let transport = HttpClient::new()?;

    eprintln!(
        "Checking {} source packages against Fedora's data for {current_tag}...",
        packages.len()
    );
    let maintainers = check_maintainers(&transport, packages, &current_tag)?;

    let fedora_packages: Vec<&InstalledPackage> = maintainers
        .statuses
        .iter()
        .map(|entry| &entry.package)
        .collect();
    let activity = check_activity(&transport, &fedora_packages, dates::today());

    print_maintainers(&maintainers);
    println!();
    print_activity(&activity);
    Ok(())
}

fn print_maintainers(report: &MaintainerReport) {
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }

    let needing_attention: Vec<&PackageStatus> = report
        .statuses
        .iter()
        .filter(|entry| entry.status != MaintainerStatus::Maintained)
        .collect();

    println!("Maintainer status");
    if needing_attention.is_empty() {
        println!("No packages need attention.");
    } else {
        println!("{}", build_table(&needing_attention));
    }
    println!("{}", summary(report));
}

fn print_activity(report: &ActivityReport) {
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }

    let not_active: Vec<&PackageActivity> = report
        .statuses
        .iter()
        .filter(|entry| !matches!(entry.status, ActivityStatus::Active { .. }))
        .collect();

    println!("Fedora activity");
    if not_active.is_empty() {
        println!("All packages have changed recently.");
    } else {
        println!("{}", build_activity_table(&not_active));
    }
    println!("{}", activity_summary(report));
}

fn build_table(entries: &[&PackageStatus]) -> Table {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header([
            "Source package",
            "Installed RPMs",
            "Version",
            "Status",
            "Reason",
        ]);
    for entry in entries {
        table.add_row([
            entry.package.source_name.clone(),
            summarize_rpms(&entry.package.binaries),
            entry.package.versions.join(", "),
            entry.status.label().to_string(),
            entry.status.reason(),
        ]);
    }
    table
}

fn build_activity_table(entries: &[&PackageActivity]) -> Table {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header([
            "Source package",
            "Installed RPMs",
            "Status",
            "Last change (excluding mass rebuilds)",
        ]);
    for entry in entries {
        table.add_row([
            entry.package.source_name.clone(),
            summarize_rpms(&entry.package.binaries),
            entry.status.label().to_string(),
            entry.status.detail(),
        ]);
    }
    table
}

/// Names the RPMs you'd actually look up or remove, which often differ from
/// the source package. Some packages have dozens, so long lists get shortened.
fn summarize_rpms(binaries: &[String]) -> String {
    if binaries.len() <= MAX_RPMS_SHOWN {
        binaries.join(", ")
    } else {
        format!(
            "{}, +{} more",
            binaries[..MAX_RPMS_SHOWN].join(", "),
            binaries.len() - MAX_RPMS_SHOWN
        )
    }
}

fn summary(report: &MaintainerReport) -> String {
    let count = |label: &str| {
        report
            .statuses
            .iter()
            .filter(|entry| entry.status.label() == label)
            .count()
    };
    format!(
        "Checked {} Fedora source packages: {} retired, {} retiring, {} orphaned, {} unknown, {} maintained. Skipped {} third-party.",
        report.statuses.len(),
        count("Retired"),
        count("Retiring"),
        count("Orphaned"),
        count("Unknown"),
        count("Maintained"),
        report.third_party.len(),
    )
}

fn activity_summary(report: &ActivityReport) -> String {
    let count = |label: &str| {
        report
            .statuses
            .iter()
            .filter(|entry| entry.status.label() == label)
            .count()
    };
    format!(
        "{} active, {} quiet, {} unknown. Quiet means no changes in two years, which can be fine for finished, stable software.",
        count("Active"),
        count("Quiet"),
        count("Unknown"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn short_rpm_lists_are_shown_in_full() {
        assert_eq!(
            summarize_rpms(&names(&["linux-atm-libs"])),
            "linux-atm-libs"
        );
    }

    #[test]
    fn long_rpm_lists_are_shortened() {
        assert_eq!(
            summarize_rpms(&names(&["a", "b", "c", "d", "e"])),
            "a, b, c, +2 more"
        );
    }
}
