use std::fmt::Display;
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use comfy_table::{ColumnConstraint, ContentArrangement, Table};
use rpm_health_core::cache::{self, CacheStats, CachingTransport};
use rpm_health_core::error::{Error, Result};
use rpm_health_core::health::{HealthLabel, HealthReport, PackageHealth, check_health};
use rpm_health_core::http::HttpClient;
use rpm_health_core::sources::anitya;
use rpm_health_core::{dates, release, rpm};

const MAX_RPMS_SHOWN: usize = 3;
/// Below this terminal width, the RPM and version columns get squeezed until
/// they're unreadable, so they're left out. `show` still has them.
const WIDE_TABLE_COLUMNS: u16 = 120;
const BINARY_NAME: &str = "rpm-health";
const CHANGELOG_ENTRIES_SHOWN: usize = 5;
const SECONDS_PER_HOUR: u64 = 60 * 60;

// `derive(Parser)` generates the argument parsing and `--help` text from this
// struct. `///` doc comments become the help text, so explanations for readers
// of the code, like this one, use `//`.

/// Finds installed Fedora packages that are at risk of dying or going stale.
#[derive(Parser)]
#[command(
    name = BINARY_NAME,
    version,
    after_long_help = "\
rpm-health only reads. It never changes your installed packages. The one thing it
writes is its cache of responses from Fedora's public services, which is kept for
a day in $XDG_CACHE_HOME/rpm-health, or ~/.cache/rpm-health if that isn't set."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check all installed packages and list the ones that need attention
    ///
    /// Each Fedora package gets one label, from three signals: whether it has a
    /// maintainer, when it last changed, and how far it is behind upstream. Old
    /// doesn't mean unhealthy, so a quiet package that matches upstream is healthy.
    ///
    /// Retired: removed from your Fedora release.
    ///
    /// Orphaned: has no maintainer, and will be retired unless someone adopts it.
    ///
    /// At risk: removed from Rawhide, so it won't be in the next Fedora release.
    ///
    /// Stale: quiet for two years and behind upstream, or a major version behind
    /// upstream even in Rawhide, once that major version is at least 90 days old.
    ///
    /// Unknown: there isn't enough information for a verdict.
    ///
    /// Healthy: none of the above.
    ///
    /// Healthy and Unknown packages aren't listed unless requested with --only, but
    /// they're always counted in the summary.
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
    ///
    /// Shows the label, every signal, and recent changelog entries. Packages can
    /// be named by RPM or by source package.
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
    /// Print a shell completion script
    ///
    /// For example, for bash:
    /// rpm-health completions bash > ~/.local/share/bash-completion/completions/rpm-health
    Completions {
        /// The shell to generate for
        shell: Shell,
    },
    /// Write man pages to a directory, for packaging
    #[command(hide = true)]
    Man {
        /// Where to write the pages
        dir: PathBuf,
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
        Command::Completions { shell } => {
            emit(completion_script(shell));
            Ok(())
        }
        Command::Man { dir } => write_man_pages(&dir),
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
        eprintln!("Nothing recent is cached, so this takes about 10 minutes...");
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
        let narrow = terminal_is_narrow();
        emit(build_table(&selected, narrow));
        if narrow {
            emit("Run rpm-health show <package> for its RPMs and versions.");
        }
    }
    emit(summary(&report));
    if only.is_none()
        && let Some(hint) = unknown_hint(&report)
    {
        emit(hint);
    }
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

/// `CommandFactory` gives us clap's description of the command line, which is
/// also what the completions and man pages are built from. That means they can't
/// drift out of step with what the program really accepts.
fn completion_script(shell: Shell) -> String {
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), BINARY_NAME, &mut script);
    String::from_utf8_lossy(&script).into_owned()
}

fn write_man_pages(dir: &Path) -> Result<()> {
    let to_error = |e: io::Error| Error::Output(format!("{}: {e}", dir.display()));
    fs::create_dir_all(dir).map_err(to_error)?;
    clap_mangen::generate_to(Cli::command(), dir).map_err(to_error)
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

/// With no filter, only packages you might act on are listed. Healthy ones aren't
/// a concern, and Unknown ones are mostly things like fonts and macro packages with
/// no upstream to compare against. The most urgent packages come first.
fn select(report: &HealthReport, only: Option<HealthLabel>) -> Vec<&PackageHealth> {
    let mut selected: Vec<&PackageHealth> = report
        .packages
        .iter()
        .filter(|entry| match only {
            Some(label) => entry.label == label,
            None => !matches!(entry.label, HealthLabel::Healthy | HealthLabel::Unknown),
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

/// `width()` is the terminal's width, or `None` when output is going to a file or
/// a pipe. In that case the full table is the most useful.
fn terminal_is_narrow() -> bool {
    Table::new()
        .width()
        .is_some_and(|width| width < WIDE_TABLE_COLUMNS)
}

fn build_table(entries: &[&PackageHealth], narrow: bool) -> Table {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    if narrow {
        table.set_header(["Source package", "Health", "Why"]);
        for entry in entries {
            table.add_row([
                entry.package.source_name.clone(),
                entry.label.name().to_string(),
                entry.reason.clone(),
            ]);
        }
    } else {
        table.set_header([
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
    }
    // Package names are what people copy, so they're never split across lines.
    if let Some(name_column) = table.column_mut(0) {
        name_column.set_constraint(ColumnConstraint::ContentWidth);
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

fn unknown_hint(report: &HealthReport) -> Option<String> {
    let unknown = report
        .packages
        .iter()
        .filter(|entry| entry.label == HealthLabel::Unknown)
        .count();
    (unknown > 0).then(|| {
        format!(
            "Not listed: {unknown} unknown, which there isn't enough data to judge. Use --only unknown to see them."
        )
    })
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
    fn unknown_packages_are_hidden_by_default_but_can_be_requested() {
        let mut scan = report();
        scan.packages.push(entry("e-unknown", HealthLabel::Unknown));

        let names = |only| -> Vec<String> {
            select(&scan, only)
                .iter()
                .map(|entry| entry.package.source_name.clone())
                .collect()
        };

        assert!(!names(None).contains(&"e-unknown".to_string()));
        assert_eq!(names(Some(HealthLabel::Unknown)), ["e-unknown"]);
        assert!(unknown_hint(&scan).unwrap().contains("1 unknown"));
    }

    #[test]
    fn there_is_no_unknown_hint_when_there_are_none() {
        assert_eq!(unknown_hint(&report()), None);
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
        let text = build_table(&select(&scan, None), false).to_string();
        assert!(text.contains("c-retired"));
        assert!(text.contains("Installed RPMs"));
        assert!(text.contains("Retired"));
        assert!(text.contains("Because"));
        assert!(!text.contains("b-healthy"));
    }

    #[test]
    fn the_narrow_table_drops_rpms_and_keeps_the_reason() {
        let scan = report();
        let text = build_table(&select(&scan, None), true).to_string();
        assert!(!text.contains("Installed RPMs"));
        assert!(!text.contains("1.0-1.fc45"));
        assert!(text.contains("Because"));
    }

    /// The real output that prompted this split names like `abattis-cantar/ell-fonts`.
    #[test]
    fn package_names_are_not_split_in_an_80_column_terminal() {
        let long_name = "abattis-cantarell-fonts";
        let mut long_entry = entry(long_name, HealthLabel::Stale);
        long_entry.reason = "No changes found back to 2023-07-19. ".repeat(4);
        let mut table = build_table(&[&long_entry], true);
        table.set_width(80);

        let text = table.to_string();

        assert!(text.contains(long_name));
        assert!(text.lines().all(|line| line.chars().count() <= 80));
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

    /// clap checks its own configuration here, catching mistakes such as two
    /// options sharing a name, which would otherwise only show up at runtime.
    #[test]
    fn the_command_line_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn completions_include_the_real_commands_and_options() {
        let script = completion_script(Shell::Bash);
        for word in ["scan", "show", "--only", "--json", "--refresh", "at-risk"] {
            assert!(script.contains(word), "completions are missing {word}");
        }
    }

    #[test]
    fn man_pages_are_written_for_each_public_command() {
        let dir = std::env::temp_dir().join(format!("rpm-health-man-{}", process::id()));
        write_man_pages(&dir).unwrap();

        let page = |name: &str| fs::read_to_string(dir.join(name));
        let main_page = page("rpm-health.1").unwrap();
        let scan_page = page("rpm-health-scan.1").unwrap();
        let hidden_page = page("rpm-health-man.1");
        fs::remove_dir_all(&dir).unwrap();

        assert!(main_page.contains("only reads"));
        assert!(scan_page.contains("Orphaned"));
        assert!(hidden_page.is_err());
    }
}
