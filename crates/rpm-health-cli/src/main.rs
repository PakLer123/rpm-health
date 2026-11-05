use std::fmt::Display;
use std::io::{self, ErrorKind, Write};
use std::process::{self, ExitCode};

use clap::{Parser, Subcommand, ValueEnum};
use comfy_table::{ContentArrangement, Table};
use rpm_health_core::cache::{self, CacheStats, CachingTransport};
use rpm_health_core::error::{Error, Result};
use rpm_health_core::health::{HealthLabel, HealthReport, PackageHealth, check_health};
use rpm_health_core::http::HttpClient;
use rpm_health_core::sources::anitya;
use rpm_health_core::{dates, release, rpm};

const MAX_RPMS_SHOWN: usize = 3;
const CHANGELOG_ENTRIES_SHOWN: usize = 5;
const SECONDS_PER_HOUR: u64 = 60 * 60;

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
        /// Print machine-readable JSON, including healthy packages by default
        #[arg(long)]
        json: bool,
        /// Fetch fresh data instead of using what's cached from the last day
        #[arg(long)]
        refresh: bool,
    },
    /// Show full details for one installed package
    Show {
        /// An RPM name or source package name
        package: String,
        /// Print machine-readable JSON
        #[arg(long)]
        json: bool,
        /// Fetch fresh data instead of using what's cached from the last day
        #[arg(long)]
        refresh: bool,
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
        Command::Scan {
            only,
            json,
            refresh,
        } => scan(only.map(HealthLabel::from), json, refresh),
        Command::Show {
            package,
            json,
            refresh,
        } => show(&package, json, refresh),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            if matches!(e, Error::Parse { .. } | Error::KojiFault(_)) {
                eprintln!("If this keeps happening, try again with --refresh.");
            }
            ExitCode::FAILURE
        }
    }
}

/// The cache sits between the checks and the real client, and neither one needs
/// to know about it.
fn build_transport(refresh: bool) -> Result<CachingTransport<HttpClient>> {
    let dir = cache::default_dir();
    if dir.is_none() {
        eprintln!("warning: Neither XDG_CACHE_HOME nor HOME is set, so nothing will be cached.");
    }
    Ok(CachingTransport::new(HttpClient::new()?, dir, refresh))
}

fn scan(only: Option<HealthLabel>, json: bool, refresh: bool) -> Result<()> {
    let current_tag = release::current_tag()?;
    let packages = rpm::list_installed()?;
    let transport = build_transport(refresh)?;

    eprintln!(
        "Checking {} source packages against Fedora's data for {current_tag}.",
        packages.len()
    );
    if !transport.has_saved_response(&anitya::full_list_url(1)?) {
        eprintln!("Nothing recent is cached, so this takes about 8 minutes...");
    }
    let mut report = check_health(&transport, packages, &current_tag, dates::today())?;

    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    print_cache_use(transport.stats());
    if json {
        // Scripts usually want everything, so only an explicit filter hides any.
        if let Some(label) = only {
            report.packages.retain(|entry| entry.label == label);
        }
        return print_json(&report);
    }
    let selected = select(&report, only);
    if selected.is_empty() {
        emit(nothing_selected_message(only));
    } else {
        emit(build_table(&selected));
    }
    emit(summary(&report));
    Ok(())
}

fn show(name: &str, json: bool, refresh: bool) -> Result<()> {
    let package = rpm::find_package(rpm::list_installed()?, name)?;
    if !package.from_fedora {
        if json {
            return print_json(&package);
        }
        emit(format!(
            "{} comes from outside Fedora, so Fedora's data can't say anything about it.",
            package.source_name
        ));
        return Ok(());
    }

    let current_tag = release::current_tag()?;
    let transport = build_transport(refresh)?;
    let report = check_health(&transport, vec![package], &current_tag, dates::today())?;

    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    print_cache_use(transport.stats());
    for entry in &report.packages {
        if json {
            print_json(entry)?;
        } else {
            emit(package_details(entry));
        }
    }
    Ok(())
}

/// Writes a line to stdout. Unlike `println!`, this doesn't panic when the reader
/// goes away, as `head` does once it has enough. That isn't an error: the reader
/// simply doesn't want more, so we stop quietly.
fn emit(text: impl Display) {
    if let Err(e) = writeln!(io::stdout(), "{text}") {
        if e.kind() == ErrorKind::BrokenPipe {
            process::exit(0);
        }
        eprintln!("error: could not write output: {e}");
        process::exit(1);
    }
}

/// `impl Serialize` accepts any type that serde can turn into JSON.
fn print_json(value: &impl serde::Serialize) -> Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(|e| Error::Parse {
        what: "report while writing JSON",
        message: e.to_string(),
    })?;
    emit(text);
    Ok(())
}

fn package_details(entry: &PackageHealth) -> String {
    let package = &entry.package;
    let mut lines = vec![
        format!(
            "{0} (https://src.fedoraproject.org/rpms/{0})",
            package.source_name
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

/// Goes to stderr like other notes, so `--json` output stays clean for scripts.
fn print_cache_use(stats: CacheStats) {
    eprintln!("{}", describe_cache_use(stats));
    if stats.failed_writes > 0 {
        eprintln!(
            "warning: Couldn't save {} responses to the cache, so the next run will be slower.",
            stats.failed_writes
        );
    }
}

fn describe_cache_use(stats: CacheStats) -> String {
    match stats.oldest_reused_seconds {
        Some(seconds) => format!(
            "Made {} requests and reused {} saved responses, the oldest from {}. Use --refresh for fresh data.",
            stats.fetched,
            stats.reused,
            describe_age(seconds),
        ),
        None => format!("Made {} requests.", stats.fetched),
    }
}

fn describe_age(seconds: u64) -> String {
    match seconds / SECONDS_PER_HOUR {
        0 => "less than an hour ago".to_string(),
        1 => "1 hour ago".to_string(),
        hours => format!("{hours} hours ago"),
    }
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
            schema_version: 1,
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
    fn cache_use_reports_how_old_the_data_is() {
        let stats = CacheStats {
            reused: 130,
            fetched: 2,
            oldest_reused_seconds: Some(5 * SECONDS_PER_HOUR),
            failed_writes: 0,
        };
        let text = describe_cache_use(stats);
        assert!(text.contains("Made 2 requests and reused 130"));
        assert!(text.contains("5 hours ago"));
    }

    #[test]
    fn cache_use_is_short_when_nothing_was_reused() {
        let stats = CacheStats {
            fetched: 7,
            ..CacheStats::default()
        };
        assert_eq!(describe_cache_use(stats), "Made 7 requests.");
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(describe_age(60), "less than an hour ago");
        assert_eq!(describe_age(SECONDS_PER_HOUR), "1 hour ago");
        assert_eq!(describe_age(23 * SECONDS_PER_HOUR), "23 hours ago");
    }

    #[test]
    fn the_table_shows_the_rpm_label_and_reason() {
        let scan = report();
        let text = build_table(&select(&scan, None)).to_string();
        assert!(text.contains("c-retired"));
        assert!(text.contains("Retired"));
        assert!(text.contains("Because"));
        assert!(!text.contains("b-healthy"));
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
