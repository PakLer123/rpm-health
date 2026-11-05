use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use comfy_table::{ContentArrangement, Table};
use rpm_health_core::error::Result;
use rpm_health_core::health::{HealthLabel, HealthReport, PackageHealth, check_health};
use rpm_health_core::http::HttpClient;
use rpm_health_core::{dates, release, rpm};

const MAX_RPMS_SHOWN: usize = 3;
const CHANGELOG_ENTRIES_SHOWN: usize = 5;

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
    Scan {
        /// Only list packages with this health label
        #[arg(long, value_enum)]
        only: Option<LabelFilter>,
    },
    /// Show full details for one installed package
    Show {
        /// An RPM name or source package name
        package: String,
    },
}

// The core library doesn't know about clap, so the command line has its own
// list of labels, and `From` converts between the two.
#[derive(Clone, Copy, ValueEnum)]
enum LabelFilter {
    Retired,
    Orphaned,
    AtRisk,
    Stale,
    Unknown,
    Healthy,
}

impl From<LabelFilter> for HealthLabel {
    fn from(filter: LabelFilter) -> Self {
        match filter {
            LabelFilter::Retired => HealthLabel::Retired,
            LabelFilter::Orphaned => HealthLabel::Orphaned,
            LabelFilter::AtRisk => HealthLabel::AtRisk,
            LabelFilter::Stale => HealthLabel::Stale,
            LabelFilter::Unknown => HealthLabel::Unknown,
            LabelFilter::Healthy => HealthLabel::Healthy,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Scan { only } => scan(only.map(HealthLabel::from)),
        Command::Show { package } => show(&package),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn scan(only: Option<HealthLabel>) -> Result<()> {
    let current_tag = release::current_tag()?;
    let packages = rpm::list_installed()?;
    let transport = HttpClient::new()?;

    eprintln!(
        "Checking {} source packages against Fedora's data for {current_tag}.",
        packages.len()
    );
    eprintln!("Reading upstream versions takes about 5 minutes without a cache...");
    let report = check_health(&transport, packages, &current_tag, dates::today())?;

    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    let selected = select(&report, only);
    if selected.is_empty() {
        println!("{}", nothing_selected_message(only));
    } else {
        println!("{}", build_table(&selected));
    }
    println!("{}", summary(&report));
    Ok(())
}

fn show(name: &str) -> Result<()> {
    let package = rpm::find_package(rpm::list_installed()?, name)?;
    if !package.from_fedora {
        println!(
            "{} comes from outside Fedora, so Fedora's data can't say anything about it.",
            package.source_name
        );
        return Ok(());
    }

    let current_tag = release::current_tag()?;
    let transport = HttpClient::new()?;
    let report = check_health(&transport, vec![package], &current_tag, dates::today())?;

    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    for entry in &report.packages {
        println!("{}", package_details(entry));
    }
    Ok(())
}

fn package_details(entry: &PackageHealth) -> String {
    let package = &entry.package;
    let mut lines = vec![
        format!(
            "{} ({})",
            package.source_name,
            format_args!("https://src.fedoraproject.org/rpms/{}", package.source_name)
        ),
        format!("  Health:      {}: {}", entry.label.name(), entry.reason),
        format!("  RPMs:        {}", package.binaries.join(", ")),
        format!("  Version:     {}", package.versions.join(", ")),
        format!(
            "  Maintainer:  {}: {}",
            entry.maintainer.label(),
            entry.maintainer.reason()
        ),
        format!(
            "  Activity:    {}: {}",
            entry.activity.label(),
            entry.activity.detail()
        ),
        format!(
            "  Upstream:    {}: {}",
            entry.upstream.label(),
            entry.upstream.detail()
        ),
    ];

    if !package.changelog.is_empty() {
        lines.push("  Recent changelog:".to_string());
        for change in package.changelog.iter().take(CHANGELOG_ENTRIES_SHOWN) {
            let first_line = change.text.lines().next().unwrap_or_default();
            lines.push(format!("    {}  {first_line}", change.date));
        }
    }
    lines.join("\n")
}

/// With no filter, healthy packages are left out so the problems stand out.
/// The most urgent packages come first.
fn select(report: &HealthReport, only: Option<HealthLabel>) -> Vec<&PackageHealth> {
    let mut selected: Vec<&PackageHealth> = report
        .packages
        .iter()
        .filter(|entry| match only {
            Some(label) => entry.label == label,
            None => entry.label != HealthLabel::Healthy,
        })
        .collect();
    // This sort keeps equal items in their existing order, which is by name.
    selected.sort_by_key(|entry| entry.label);
    selected
}

fn nothing_selected_message(only: Option<HealthLabel>) -> String {
    match only {
        Some(label) => format!("No packages are labelled {}.", label.name()),
        None => "No packages need attention.".to_string(),
    }
}

fn build_table(entries: &[&PackageHealth]) -> Table {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header([
            "Source package",
            "Installed RPMs",
            "Version",
            "Health",
            "Why",
        ]);
    for entry in entries {
        table.add_row([
            entry.package.source_name.clone(),
            summarize_rpms(&entry.package.binaries),
            entry.package.versions.join(", "),
            entry.label.name().to_string(),
            entry.reason.clone(),
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

fn summary(report: &HealthReport) -> String {
    let count = |label: HealthLabel| {
        report
            .packages
            .iter()
            .filter(|entry| entry.label == label)
            .count()
    };
    format!(
        "Checked {} Fedora source packages: {} retired, {} orphaned, {} at risk, {} stale, {} unknown, {} healthy. Skipped {} third-party.",
        report.packages.len(),
        count(HealthLabel::Retired),
        count(HealthLabel::Orphaned),
        count(HealthLabel::AtRisk),
        count(HealthLabel::Stale),
        count(HealthLabel::Unknown),
        count(HealthLabel::Healthy),
        report.third_party.len(),
    )
}

#[cfg(test)]
mod tests {
    use rpm_health_core::model::{
        ActivityStatus, InstalledPackage, MaintainerStatus, UpstreamStatus,
    };

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

    fn entry(name: &str, label: HealthLabel) -> PackageHealth {
        PackageHealth {
            package: InstalledPackage {
                source_name: name.to_string(),
                binaries: names(&[name]),
                versions: names(&["1.0-1.fc45"]),
                from_fedora: true,
                fedora_version: Some("1.0".to_string()),
                changelog: Vec::new(),
            },
            label,
            reason: "Because".to_string(),
            maintainer: MaintainerStatus::Maintained,
            activity: ActivityStatus::Unknown {
                reason: "Not relevant here".to_string(),
            },
            upstream: UpstreamStatus::NotTracked,
        }
    }

    fn report() -> HealthReport {
        HealthReport {
            release: "f45".to_string(),
            packages: vec![
                entry("a-stale", HealthLabel::Stale),
                entry("b-healthy", HealthLabel::Healthy),
                entry("c-retired", HealthLabel::Retired),
                entry("d-stale", HealthLabel::Stale),
            ],
            third_party: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn selected_names(only: Option<HealthLabel>) -> Vec<String> {
        select(&report(), only)
            .iter()
            .map(|entry| entry.package.source_name.clone())
            .collect()
    }

    #[test]
    fn by_default_healthy_is_hidden_and_the_most_urgent_comes_first() {
        assert_eq!(selected_names(None), ["c-retired", "a-stale", "d-stale"]);
    }

    #[test]
    fn only_shows_just_that_label() {
        assert_eq!(selected_names(Some(HealthLabel::Healthy)), ["b-healthy"]);
    }

    #[test]
    fn details_show_every_signal() {
        let text = package_details(&entry("demo", HealthLabel::Stale));
        assert!(text.contains("https://src.fedoraproject.org/rpms/demo"));
        assert!(text.contains("Health:      Stale: Because"));
        assert!(text.contains("Maintainer:  Maintained"));
        assert!(text.contains("Activity:    Unknown"));
        assert!(text.contains("Upstream:    Not tracked"));
    }

    #[test]
    fn the_summary_counts_every_package_even_when_filtered() {
        let text = summary(&report());
        assert!(text.contains("Checked 4 Fedora"));
        assert!(text.contains("1 retired"));
        assert!(text.contains("2 stale"));
    }
}
