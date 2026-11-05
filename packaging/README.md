# Packaging rpm-health for Fedora

`rpm-health.spec` follows the [non-crate rules](https://docs.fedoraproject.org/en-US/packaging-guidelines/Rust/#_non_crate_rust_projects)
in Fedora's Rust guidelines, because the project is a cargo workspace. That means:

- the package is called `rpm-health`, with no `rust-` prefix
- it's built from the GitHub source tarball, so it doesn't need to be on crates.io
- it builds against the crates Fedora already packages, with nothing vendored
- it doesn't use `rust2rpm` or `%cargo_install`, which are aimed at crates

Everything below builds on a tagged release, and the spec's `Source` URL only works
once that tag exists on GitHub.

## 1. Tag a release

```console
$ git switch main
$ git tag -a v0.1.1 -m "rpm-health 0.1.1"
$ git push origin main v0.1.1
```

## 2. Build it locally in mock

`mock` builds in a clean environment, the same way Fedora's build system does.

```console
$ sudo dnf install mock rpmdevtools fedora-review fedpkg copr-cli
$ sudo usermod -a -G mock "$USER"     # then log out and back in
$ spectool -g -C ~/rpmbuild/SOURCES packaging/rpm-health.spec
$ rpmbuild -bs packaging/rpm-health.spec
$ mock -r fedora-rawhide-x86_64 --rebuild ~/rpmbuild/SRPMS/rpm-health-0.1.1-1.*.src.rpm
```

Results are written to `/var/lib/mock/fedora-rawhide-x86_64/result/`.

## 3. Publish to COPR

COPR lets people install the package before it's in Fedora, and gives reviewers
somewhere to download it from.

1. Create a [Fedora account](https://accounts.fedoraproject.org/).
2. Log in at <https://copr.fedorainfracloud.org/api/> and save the token it gives you
   as `~/.config/copr`.
3. Create the project and build it:

```console
$ copr-cli create rpm-health --chroot fedora-45-x86_64 --chroot fedora-rawhide-x86_64 \
    --description "Find installed Fedora packages at risk of dying or going stale"
$ copr-cli build rpm-health ~/rpmbuild/SRPMS/rpm-health-0.1.1-1.*.src.rpm
```

People can then install it with:

```console
$ sudo dnf copr enable <your-fedora-username>/rpm-health
$ sudo dnf install rpm-health
```

## 4. Request a Fedora package review

The full process is in the
[Package Review Process](https://docs.fedoraproject.org/en-US/package-maintainers/Package_Review_Process/)
and [Joining the Package Maintainers](https://docs.fedoraproject.org/en-US/package-maintainers/Joining_the_Package_Maintainers/).

### Before you submit

- **Make the accounts match.** Your [Bugzilla](https://bugzilla.redhat.com/) and Fedora
  accounts must use the same email address, or the request can be closed as invalid.
- **Use your real name.** Fedora asks for this when communicating with the community.
  Put it in the spec's `%changelog` entry as well.
- **Be ready to maintain it.** Maintainers must fix bugs in a timely manner, especially
  security issues, and respond to review comments or the ticket gets closed.
- **Be able to explain all of it.** See the next section.

### AI assistance must be disclosed

This project was built with substantial help from an AI tool. Fedora's
[AI-Assisted Contributions Policy](https://docs.fedoraproject.org/en-US/council/policy/ai-contribution-policy/)
allows that, with two conditions:

- **Accountability:** the contributor is the author and is fully accountable for the
  whole contribution, including its quality and license compliance.
- **Transparency:** significant unmodified AI-generated work must be disclosed. In git,
  the recommended form is an `Assisted-by:` commit trailer.

So say so in the review request, using the text below. A sponsor is deciding whether
to trust you as a packager, so expect questions about how the code and spec work.

### Submit it

1. Run the automated review, and fix anything it reports. It looks for `rpm-health.spec`
   and the source RPM in the current directory:

   ```console
   $ fedora-review -n rpm-health
   ```

2. Put the spec and source RPM somewhere they can be downloaded directly. A COPR
   build provides this.
3. File a [review request in Bugzilla](https://bugzilla.redhat.com/enter_bug.cgi?product=Fedora&format=fedora-review)
   for `rpm-health`, using the template below.
4. **Add `FE-NEEDSPONSOR` to the "Blocks" field**, because a first-time packager needs a sponsor.
5. Introduce yourself on the
   [devel mailing list](https://lists.fedoraproject.org/admin/lists/devel.lists.fedoraproject.org/)
   with a link to the request, using the subject `Self Introduction: <your name>`.
6. If nobody responds, ask for a review swap on the devel list or in Fedora Discussion's
   Package Review Swaps category. You can also
   [ask for a sponsor](https://docs.fedoraproject.org/en-US/package-maintainers/How_to_Get_Sponsored_into_the_Packager_Group/).

### Review request template

```text
Spec URL: <direct link to rpm-health.spec>
SRPM URL: <direct link to the .src.rpm>
Description: rpm-health tells you which of your installed Fedora packages are at
risk of dying, going stale, or falling behind upstream, and explains why.
Fedora Account System Username: <your username>

This is my first package, and I need a sponsor.

COPR build: <link to your COPR project>

Disclosure: this project and its spec file were written with substantial assistance
from an AI tool (Claude). I have reviewed the work and take responsibility for it.
```

Only keep the last sentence if it's true by then.

### After approval

When a reviewer sets `fedora-review` to `+`, and you've been sponsored:

```console
$ fedpkg request-repo rpm-health <bug number>
$ fedpkg clone rpm-health
```

Then import the source RPM, run `fedpkg build`, and consider enabling
[Upstream Release Monitoring](https://docs.fedoraproject.org/en-US/package-maintainers/Upstream_Release_Monitoring/).
