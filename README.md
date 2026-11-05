# rpm-health

Tells you which of your installed Fedora packages are at risk of dying, going
stale, or falling behind upstream, and explains why.

```console
$ rpm-health show pkgconf
pkgconf (https://src.fedoraproject.org/rpms/pkgconf)
  Health:      Stale: A major version behind. Upstream is at 3.0.10, and Rawhide has 2.5.1
  RPMs:        libpkgconf, pkgconf, pkgconf-m4, pkgconf-pkg-config
  Version:     2.5.1-2.fc45
  Maintainer:  Maintained: Has a maintainer
  Activity:    Active: 2026-02-12
  Upstream:    Behind: Upstream is at 3.0.10, and Rawhide has 2.5.1
  Recent changelog:
    2026-07-16  - Rebuilt for https://fedoraproject.org/wiki/Fedora_45_Mass_Rebuild
    2026-02-12  - Update to 2.5.1
    ...
```

Fedora publishes all of this information, but it's spread across several services
and organised around maintainers. rpm-health starts from what's installed on your
machine and gives each package one verdict with a reason.

It's **read-only**. It never changes your installed packages. The only thing it
writes is a cache of responses in your own cache directory.

## Installing

On Fedora 45 or Rawhide, install it from [COPR](https://copr.fedorainfracloud.org/coprs/pakler123/rpm-health/):

```console
$ sudo dnf copr enable pakler123/rpm-health
$ sudo dnf install rpm-health
```

This includes the man pages and shell completions. COPR is a community build service,
so this isn't an official Fedora package yet.

To build from a checkout of this repository instead:

```console
$ sudo dnf install cargo gcc openssl-devel
$ cargo install --locked --path crates/rpm-health-cli
```

[`packaging/`](packaging/README.md) has the RPM spec and packaging steps.

## Usage

```console
$ rpm-health scan                 # list the packages that need attention
$ rpm-health scan --only stale    # list only one label
$ rpm-health scan --json          # everything, for scripts
$ rpm-health show linux-atm-libs  # full details, by RPM or source package name
$ rpm-health scan --refresh       # ignore the cache and fetch fresh data
```

The first scan of the day takes about 8 minutes, because one of the data sources
has no way to look up many packages at once. Later scans reuse saved responses and
take a couple of seconds.

### Shell completions and man pages

The COPR package already includes these. If you built from source, install them with:

```console
$ mkdir -p ~/.local/share/bash-completion/completions
$ rpm-health completions bash > ~/.local/share/bash-completion/completions/rpm-health
$ rpm-health man ~/.local/share/man/man1
```

Completions are also available for `zsh`, `fish`, `elvish` and `powershell`.

## What the labels mean

Old doesn't mean unhealthy. A finished, stable tool can go years without needing
changes, so no single signal decides unless it's definitive.

| Label | Meaning |
|---|---|
| **Retired** | Removed from your Fedora release, so it no longer gets updates |
| **Orphaned** | Has no maintainer, and will be retired after six weeks unless adopted |
| **At risk** | Removed from Rawhide, so it won't be in the next Fedora release |
| **Stale** | Quiet for two years and behind upstream, or a major version behind upstream even in Rawhide |
| **Unknown** | Quiet, with no usable upstream data, so there's no evidence either way |
| **Healthy** | None of the above |

"Quiet" means no changes in two years, not counting the mass rebuilds Fedora does
for every package.

Packages from outside Fedora, such as RPM Fusion or COPR, are skipped and counted,
because Fedora's data says nothing about them.

## Where the data comes from

| Question | Source | Requests |
|---|---|---|
| Installed packages and their changelogs | The local RPM database | None |
| Retired or orphaned? | [Koji](https://koji.fedoraproject.org/), Fedora's build system | Batched |
| Why was it retired? | `dead.package` in [Fedora's package repositories](https://src.fedoraproject.org/) | Only for retired packages |
| When was it orphaned? | The report behind Fedora's orphan announcements | One |
| When did it last change? | The local changelog, then Koji for quiet packages | Batched |
| What's the latest upstream release? | [release-monitoring.org](https://release-monitoring.org/) (Anitya) | About 100 pages |
| Which release is Rawhide? | [Bodhi](https://bodhi.fedoraproject.org/), Fedora's update system | One |

### Being a good neighbour

These are shared community services, so rpm-health:

- sends a descriptive `User-Agent`
- caches responses for a day
- batches requests where the APIs allow it, and asks for only what it needs
- makes one request at a time, with a pause between them
- retries a limited number of times, and honours `Retry-After`

## Known limitations

- **Upstream data isn't complete.** release-monitoring.org doesn't track every
  package, and it's sometimes wrong or out of date. When versions can't be compared
  fairly, rpm-health says so instead of guessing, but active packages can still be
  labelled Healthy without an upstream comparison. `show` tells you when that's
  the case.
- **Activity is a generous measure.** A rebuild for a new Python or library version
  counts as activity, because Koji can't tell it apart from other work. So a package
  can look active without recent attention from a maintainer.
- **Results can be up to a day old**, unless you use `--refresh`.
- **The orphan report is unofficial.** It's undocumented and hosted outside Fedora's
  infrastructure. If it's unavailable, you still get orphan status from Koji, just
  without the dates.

## Development notes

This project was built with substantial assistance from an AI tool (Claude), under
the direction of its maintainer. See [Fedora's policy](https://docs.fedoraproject.org/en-US/council/policy/ai-contribution-policy/)
on AI-assisted contributions.

## Related tools

If rpm-health isn't what you're after, one of these might be:

- [`sandogasa-pkg-health`](https://github.com/slopfest/sandogasa) audits a TOML
  inventory of packages, and is aimed at maintainers and SIGs
- [Fedora Packager Dashboard](https://packager-dashboard.fedoraproject.org/) is a
  website organised around maintainers
- `remove-retired-packages` finds retired packages when upgrading between releases,
  and offers to remove them
- `dnf list --extras` lists installed packages that are in no repository, which
  includes third-party ones as well as retired ones

## Development

The logic lives in `rpm-health-core`, which never prints anything, and
`rpm-health-cli` is a thin front-end. That should make a future GTK front-end a new
crate rather than a rewrite.

```text
crates/rpm-health-core/src/
  rpm.rs, release.rs   what's installed on this system
  http.rs, cache.rs    the swappable HTTP layer and its disk cache
  sources/             one client per external service
  signals/             maintainer, activity and upstream checks
  version.rs           comparing versions fairly
  health.rs            combining the signals into a label
crates/rpm-health-cli/src/main.rs
```

Tests use saved real responses in `crates/rpm-health-core/tests/fixtures/`, and make
no network requests. Before sending a change, run the same checks as CI:

```console
$ cargo fmt --check
$ cargo clippy --all-targets --workspace -- -D warnings
$ cargo test --workspace
```

## Roadmap

- A Fedora package review request
- A systemd timer and desktop notifications
- A GTK4 and libadwaita front-end

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.
