%bcond check 1

Name:           rpm-health
Version:        0.1.1
Release:        1%{?dist}
Summary:        Find installed Fedora packages at risk of dying or going stale

SourceLicense:  Apache-2.0 OR MIT
# The binary statically links its crate dependencies. To update this list, read
# the %%cargo_license_summary output in the build log:
#
# Apache-2.0
# Apache-2.0 OR MIT
# Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT
# MIT
# MIT OR Apache-2.0
# Unicode-3.0
# Unlicense OR MIT
License:        Apache-2.0 AND (Apache-2.0 OR MIT) AND (Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT) AND MIT AND Unicode-3.0 AND (Unlicense OR MIT)
URL:            https://github.com/PakLer123/rpm-health
Source:         %{url}/archive/v%{version}/%{name}-%{version}.tar.gz

ExclusiveArch:  %{rust_arches}

BuildRequires:  cargo-rpm-macros >= 26

# Lists installed packages by running the rpm command.
Requires:       rpm

%description
rpm-health tells you which of your installed Fedora packages are at risk of
dying, going stale, or falling behind upstream, and explains why. Each package
gets one verdict, based on whether it has a maintainer, when it last changed,
and how far it is behind the latest upstream release.

It never changes your installed packages. It reads the local RPM database and
queries public Fedora services, keeping a cache in your own cache directory.

%prep
%autosetup -n %{name}-%{version} -p1
%cargo_prep
# The lock file pins upstream's exact versions. Fedora builds against the
# versions of each crate that it packages, so the file has to be resolved again.
rm -f Cargo.lock

%generate_buildrequires
%cargo_generate_buildrequires

%build
%cargo_build
%{cargo_license_summary}
%{cargo_license} > LICENSE.dependencies

%install
# This is a workspace rather than a crate, so the guidelines say to install the
# binary directly instead of using %%cargo_install.
install -Dpm 0755 target/rpm/%{name} %{buildroot}%{_bindir}/%{name}

mkdir -p %{buildroot}%{_mandir}/man1
target/rpm/%{name} man %{buildroot}%{_mandir}/man1

install -d %{buildroot}%{bash_completions_dir}
target/rpm/%{name} completions bash > %{buildroot}%{bash_completions_dir}/%{name}
install -d %{buildroot}%{zsh_completions_dir}
target/rpm/%{name} completions zsh > %{buildroot}%{zsh_completions_dir}/_%{name}
install -d %{buildroot}%{fish_completions_dir}
target/rpm/%{name} completions fish > %{buildroot}%{fish_completions_dir}/%{name}.fish

%if %{with check}
%check
%cargo_test
%endif

%files
%license LICENSE-APACHE LICENSE-MIT LICENSE.dependencies
%doc README.md
%{_bindir}/%{name}
%{_mandir}/man1/%{name}*.1*
%{bash_completions_dir}/%{name}
%{zsh_completions_dir}/_%{name}
%{fish_completions_dir}/%{name}.fish

%changelog
* Fri Nov 06 2026 PakLer123 <derrelted04@gmail.com> - 0.1.1-1
- Update to 0.1.1
- Give new upstream major versions a 90-day grace period before calling them stale
- Hide unknown packages from the default scan output
- Fit narrow terminals

* Fri Nov 06 2026 PakLer123 <derrelted04@gmail.com> - 0.1.0-1
- Initial package
