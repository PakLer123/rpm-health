use std::process::ExitCode;

use clap::{Parser, Subcommand};
use comfy_table::{ContentArrangement, Table};
use rpm_health_core::error::Result;
use rpm_health_core::http::HttpClient;
use rpm_health_core::model::{MaintainerStatus, PackageStatus};
use rpm_health_core::signals::maintainer::{MaintainerReport, check_maintainers};
use rpm_health_core::{release, rpm};

/// Finds installed Fedora packages that are at risk of dying or going stale.
///
/// `derive(Parser)` generates the argument parsing and `--help` text from this
/// struct, using these doc comments as the descriptions.
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
    let report = check_maintainers(&transport, packages, &current_tag)?;

    print_report(&report);
    Ok(())
}

fn print_report(report: &MaintainerReport) {
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }

    let needing_attention: Vec<&PackageStatus> = report
        .statuses
        .iter()
        .filter(|entry| entry.status != MaintainerStatus::Maintained)
        .collect();

    if needing_attention.is_empty() {
        println!("No packages need attention.");
    } else {
        println!("{}", build_table(&needing_attention));
    }
    println!("{}", summary(report));
}

fn build_table(entries: &[&PackageStatus]) -> Table {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(["Source package", "Installed", "Status", "Reason"]);
    for entry in entries {
        table.add_row([
            entry.package.source_name.clone(),
            entry.package.versions.join(", "),
            entry.status.label().to_string(),
            entry.status.reason(),
        ]);
    }
    table
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
