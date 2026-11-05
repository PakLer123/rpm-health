//! Koji, Fedora's build system, knows the state of every package per release.
//! A package that is `blocked` in a release's tag won't ship in that release.

use std::collections::HashMap;

use quick_xml::escape::escape;
use time::Date;

use crate::dates::date_from_unix;
use crate::error::{Error, Result};
use crate::http::Transport;
use crate::sources::xmlrpc::{self, Value};

const HUB_URL: &str = "https://koji.fedoraproject.org/kojihub";
/// 200 lookups come to about 150 KB and take a few seconds. Fetching the whole
/// tag instead would download 28 MB, so we only ever ask about what's installed.
const BATCH_SIZE: usize = 200;
/// Build records are much bigger than package records, at about 2.7 KB each.
const BUILDS_BATCH_SIZE: usize = 50;
/// A quiet package gets about two mass rebuilds a year, so eight builds usually
/// reaches back past them to the last build by a person.
const BUILDS_PER_PACKAGE: usize = 8;
/// Only Fedora builds, not ELN, EPEL or Flatpak builds like `fc45app1`, which
/// would otherwise use up the `BUILDS_PER_PACKAGE` window.
const FEDORA_BUILD_PATTERN: &str = "*.fc??";
/// How Koji says it has never heard of a package. That's a normal answer, unlike
/// its other faults.
const UNKNOWN_PACKAGE_FAULT: &str = "No such entry";
/// Koji assigns packages with no maintainer to this special user.
pub const ORPHAN_OWNER: &str = "orphan";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KojiPackage {
    pub blocked: bool,
    pub owner: String,
}

impl KojiPackage {
    pub fn is_orphaned(&self) -> bool {
        self.owner == ORPHAN_OWNER
    }
}

/// A successful build of a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KojiBuild {
    pub owner: String,
    pub release: String,
    pub completed: Date,
}

/// Looks up packages in a release tag such as `f45`. Packages Koji has never
/// heard of are simply absent from the returned map.
pub fn list_packages(
    transport: &dyn Transport,
    tag: &str,
    names: &[String],
) -> Result<HashMap<String, KojiPackage>> {
    let mut found = HashMap::new();
    for_each_result(
        transport,
        names,
        BATCH_SIZE,
        |name| list_packages_call(tag, name),
        |name, result| {
            if let Some(package) = read_package(result)? {
                found.insert(name.to_string(), package);
            }
            Ok(())
        },
    )?;
    Ok(found)
}

/// Fetches each package's most recent Fedora builds, newest first.
pub fn recent_builds(
    transport: &dyn Transport,
    names: &[String],
) -> Result<HashMap<String, Vec<KojiBuild>>> {
    let mut found = HashMap::new();
    for_each_result(
        transport,
        names,
        BUILDS_BATCH_SIZE,
        list_builds_call,
        |name, result| {
            found.insert(name.to_string(), read_builds(result)?);
            Ok(())
        },
    )?;
    Ok(found)
}

/// Returns the newest version of each package in a tag, such as `f46` for
/// Rawhide. Packages that have no build in that tag are absent from the map.
pub fn latest_versions(
    transport: &dyn Transport,
    tag: &str,
    names: &[String],
) -> Result<HashMap<String, String>> {
    let mut found = HashMap::new();
    for_each_result(
        transport,
        names,
        BATCH_SIZE,
        |name| latest_build_call(tag, name),
        |name, result| {
            let rows = read_rows(result)?.unwrap_or_default();
            if let Some(row) = rows.first() {
                let version = row
                    .get("version")
                    .and_then(Value::as_str)
                    .ok_or_else(|| parse_error("build row is missing version"))?;
                found.insert(name.to_string(), version.to_string());
            }
            Ok(())
        },
    )?;
    Ok(found)
}

/// Whether a response is a complete, successful answer. Koji reports errors with
/// a normal HTTP 200, so the status alone can't tell us that.
pub fn is_reusable_response(body: &str) -> bool {
    let Ok(Value::Array(results)) = xmlrpc::parse_response(body) else {
        return false;
    };
    results.iter().all(|result| {
        result.get("faultString").is_none()
            || xmlrpc::fault_message(result).starts_with(UNKNOWN_PACKAGE_FAULT)
    })
}

/// Sends one multicall per batch and passes each package's answer to `handle`.
///
/// `impl Fn` accepts any closure with this shape. `handle` is `FnMut` because
/// the closures above change the `found` map they captured.
fn for_each_result(
    transport: &dyn Transport,
    names: &[String],
    batch_size: usize,
    make_call: impl Fn(&str) -> String,
    mut handle: impl FnMut(&str, &Value) -> Result<()>,
) -> Result<()> {
    // One request at a time, on purpose. Koji is shared infrastructure.
    for batch in names.chunks(batch_size) {
        let calls: String = batch.iter().map(|name| make_call(name)).collect();
        let body = transport
            .post_xml(HUB_URL, multicall_request(&calls))?
            .into_success_body(HUB_URL)?;
        let response = xmlrpc::parse_response(&body)?;
        let results = response
            .as_array()
            .ok_or_else(|| parse_error("multicall result is not an array"))?;
        if results.len() != batch.len() {
            return Err(parse_error(format!(
                "asked about {} packages but got {} answers",
                batch.len(),
                results.len()
            )));
        }
        for (name, result) in batch.iter().zip(results) {
            handle(name, result)?;
        }
    }
    Ok(())
}

/// Each answer is either a fault struct or `[[ {row}, ... ]]`. Returns the rows,
/// or `None` if Koji doesn't know the package.
fn read_rows(result: &Value) -> Result<Option<&[Value]>> {
    if result.get("faultString").is_some() {
        let message = xmlrpc::fault_message(result);
        return if message.starts_with(UNKNOWN_PACKAGE_FAULT) {
            Ok(None)
        } else {
            Err(Error::KojiFault(message))
        };
    }

    result
        .as_array()
        .and_then(|outer| outer.first())
        .and_then(Value::as_array)
        .map(Some)
        .ok_or_else(|| parse_error("unexpected shape for a result"))
}

fn read_package(result: &Value) -> Result<Option<KojiPackage>> {
    let Some(row) = read_rows(result)?.and_then(<[Value]>::first) else {
        return Ok(None); // Koji knows the package, but not in this tag.
    };

    let blocked = row.get("blocked").and_then(Value::as_bool);
    let owner = row.get("owner_name").and_then(Value::as_str);
    match (blocked, owner) {
        (Some(blocked), Some(owner)) => Ok(Some(KojiPackage {
            blocked,
            owner: owner.to_string(),
        })),
        _ => Err(parse_error("package row is missing blocked or owner_name")),
    }
}

fn read_builds(result: &Value) -> Result<Vec<KojiBuild>> {
    let rows = read_rows(result)?.unwrap_or_default();
    rows.iter().filter_map(read_build).collect()
}

/// `None` skips a build with no completion time, which isn't worth failing for.
/// A row missing its other fields means the format changed, so that's an error.
fn read_build(row: &Value) -> Option<Result<KojiBuild>> {
    let seconds = row.get("completion_ts").and_then(Value::as_f64)?;
    let completed = date_from_unix(seconds as i64)?;
    let owner = row.get("owner_name").and_then(Value::as_str);
    let release = row.get("release").and_then(Value::as_str);
    Some(match (owner, release) {
        (Some(owner), Some(release)) => Ok(KojiBuild {
            owner: owner.to_string(),
            release: release.to_string(),
            completed,
        }),
        _ => Err(parse_error("build row is missing owner_name or release")),
    })
}

fn parse_error(message: impl Into<String>) -> Error {
    Error::Parse {
        what: "Koji package data",
        message: message.into(),
    }
}

fn multicall_request(calls: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?><methodCall><methodName>system.multicall</methodName>\
         <params><param><value><array><data>{calls}</data></array></value></param></params>\
         </methodCall>"
    )
}

fn list_packages_call(tag: &str, name: &str) -> String {
    // `__starstar` tells Koji to treat this struct as named arguments.
    format!(
        "<value><struct>\
         <member><name>methodName</name><value><string>listPackages</string></value></member>\
         <member><name>params</name><value><array><data><value><struct>\
         <member><name>tagID</name><value><string>{tag}</string></value></member>\
         <member><name>pkgID</name><value><string>{name}</string></value></member>\
         <member><name>inherited</name><value><boolean>1</boolean></value></member>\
         <member><name>with_blocked</name><value><boolean>1</boolean></value></member>\
         <member><name>__starstar</name><value><boolean>1</boolean></value></member>\
         </struct></value></data></array></value></member>\
         </struct></value>",
        tag = escape(tag),
        name = escape(name),
    )
}

fn list_builds_call(name: &str) -> String {
    format!(
        "<value><struct>\
         <member><name>methodName</name><value><string>listBuilds</string></value></member>\
         <member><name>params</name><value><array><data><value><struct>\
         <member><name>packageID</name><value><string>{name}</string></value></member>\
         <member><name>state</name><value><int>1</int></value></member>\
         <member><name>pattern</name><value><string>{FEDORA_BUILD_PATTERN}</string></value></member>\
         <member><name>queryOpts</name><value><struct>\
         <member><name>limit</name><value><int>{BUILDS_PER_PACKAGE}</int></value></member>\
         <member><name>order</name><value><string>-completion_time</string></value></member>\
         </struct></value></member>\
         <member><name>__starstar</name><value><boolean>1</boolean></value></member>\
         </struct></value></data></array></value></member>\
         </struct></value>",
        name = escape(name),
    )
}

fn latest_build_call(tag: &str, name: &str) -> String {
    format!(
        "<value><struct>\
         <member><name>methodName</name><value><string>getLatestBuilds</string></value></member>\
         <member><name>params</name><value><array><data><value><struct>\
         <member><name>tag</name><value><string>{tag}</string></value></member>\
         <member><name>package</name><value><string>{name}</string></value></member>\
         <member><name>__starstar</name><value><boolean>1</boolean></value></member>\
         </struct></value></data></array></value></member>\
         </struct></value>",
        tag = escape(tag),
        name = escape(name),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::FakeTransport;

    /// A real response from Koji, for htop, linux-atm, 389-admin, and a
    /// package name that doesn't exist.
    const FIXTURE: &str = include_str!("../../tests/fixtures/koji_multicall.xml");

    fn fixture_names() -> Vec<String> {
        [
            "htop",
            "linux-atm",
            "389-admin",
            "rpm-health-no-such-package",
        ]
        .map(String::from)
        .to_vec()
    }

    #[test]
    fn reads_blocked_and_owner_from_a_real_response() {
        let transport = FakeTransport::default().with_post_reply(FIXTURE);
        let found = list_packages(&transport, "f45", &fixture_names()).unwrap();

        assert!(!found["htop"].blocked);
        // Retired, yet still owned by a person. The owner alone isn't enough.
        assert!(found["linux-atm"].blocked);
        assert!(!found["linux-atm"].is_orphaned());
        assert!(found["389-admin"].blocked);
        assert!(found["389-admin"].is_orphaned());
    }

    #[test]
    fn unknown_packages_are_absent_rather_than_an_error() {
        let transport = FakeTransport::default().with_post_reply(FIXTURE);
        let found = list_packages(&transport, "f45", &fixture_names()).unwrap();
        assert!(!found.contains_key("rpm-health-no-such-package"));
    }

    #[test]
    fn splits_large_requests_into_batches() {
        let names: Vec<String> = (0..201).map(|i| format!("pkg{i}")).collect();
        let empty_answer = |count: usize| {
            let values = "<value><array><data><value><array><data></data></array></value></data></array></value>"
                .repeat(count);
            format!(
                "<methodResponse><params><param><value><array><data>{values}</data></array></value></param></params></methodResponse>"
            )
        };
        let transport = FakeTransport::default()
            .with_post_reply(&empty_answer(200))
            .with_post_reply(&empty_answer(1));

        let found = list_packages(&transport, "f45", &names).unwrap();

        assert!(found.is_empty());
        assert_eq!(transport.post_bodies.borrow().len(), 2);
    }

    #[test]
    fn escapes_package_names_in_requests() {
        assert!(list_packages_call("f45", "a&b").contains("a&amp;b"));
        assert!(list_builds_call("a&b").contains("a&amp;b"));
    }

    #[test]
    fn a_wrong_number_of_answers_is_an_error() {
        let transport = FakeTransport::default().with_post_reply(FIXTURE);
        let result = list_packages(&transport, "f45", &["htop".to_string()]);
        assert!(matches!(result, Err(Error::Parse { .. })));
    }

    #[test]
    fn other_faults_are_reported() {
        let fault = Value::Struct(HashMap::from([(
            "faultString".to_string(),
            Value::String("database is down".to_string()),
        )]));
        assert!(matches!(read_package(&fault), Err(Error::KojiFault(_))));
    }

    /// A real response for a package with mass rebuilds, then an unknown name.
    const BUILDS_FIXTURE: &str = include_str!("../../tests/fixtures/koji_list_builds.xml");

    #[test]
    fn reads_builds_from_a_real_response() {
        let transport = FakeTransport::default().with_post_reply(BUILDS_FIXTURE);
        let names = ["aajohan-comfortaa-fonts", "rpm-health-no-such-package"].map(String::from);

        let found = recent_builds(&transport, &names).unwrap();

        let builds = &found["aajohan-comfortaa-fonts"];
        assert_eq!(builds.len(), 8);
        assert_eq!(builds[0].owner, "releng");
        assert_eq!(builds[0].completed.to_string(), "2026-07-15");
        assert_eq!(builds[5].owner, "luya");
        assert!(found["rpm-health-no-such-package"].is_empty());
    }

    #[test]
    fn a_build_with_no_completion_time_is_skipped() {
        let row = Value::Struct(HashMap::from([("completion_ts".to_string(), Value::Nil)]));
        assert!(read_build(&row).is_none());
    }

    #[test]
    fn a_build_missing_its_owner_is_an_error() {
        let row = Value::Struct(HashMap::from([(
            "completion_ts".to_string(),
            Value::Double(1784145625.1),
        )]));
        assert!(matches!(read_build(&row), Some(Err(Error::Parse { .. }))));
    }

    /// A real response for htop, vim, and a name that doesn't exist.
    const LATEST_FIXTURE: &str = include_str!("../../tests/fixtures/koji_latest_builds.xml");

    #[test]
    fn reads_latest_versions_from_a_real_response() {
        let transport = FakeTransport::default().with_post_reply(LATEST_FIXTURE);
        let names = ["htop", "vim", "rpm-health-no-such-package"].map(String::from);

        let versions = latest_versions(&transport, "f45-updates", &names).unwrap();

        assert_eq!(versions["htop"], "3.5.3");
        assert_eq!(versions["vim"], "9.2.1202");
        assert_eq!(versions.len(), 2);
    }

    fn multicall_with(value: &str) -> String {
        format!(
            "<methodResponse><params><param><value><array><data>{value}</data></array></value>\
             </param></params></methodResponse>"
        )
    }

    fn fault_value(message: &str) -> String {
        format!(
            "<value><struct><member><name>faultString</name>\
             <value><string>{message}</string></value></member></struct></value>"
        )
    }

    /// The real fixture includes a package Koji doesn't know, which is fine.
    #[test]
    fn real_responses_are_reusable() {
        assert!(is_reusable_response(FIXTURE));
        assert!(is_reusable_response(BUILDS_FIXTURE));
        assert!(is_reusable_response(LATEST_FIXTURE));
    }

    #[test]
    fn unknown_package_answers_are_reusable() {
        let body = multicall_with(&fault_value("No such entry in table package: x"));
        assert!(is_reusable_response(&body));
    }

    #[test]
    fn other_faults_and_junk_are_not_reusable() {
        let fault = multicall_with(&fault_value("database is down"));
        assert!(!is_reusable_response(&fault));
        assert!(!is_reusable_response("<html>Bad gateway</html>"));
        assert!(!is_reusable_response(
            "<methodResponse><fault><value><struct></struct></value></fault></methodResponse>"
        ));
    }
}
