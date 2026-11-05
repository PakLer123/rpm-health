//! Koji, Fedora's build system, knows the state of every package per release.
//! A package that is `blocked` in a release's tag won't ship in that release.

use std::collections::HashMap;

use quick_xml::escape::escape;

use crate::error::{Error, Result};
use crate::http::Transport;
use crate::sources::xmlrpc::{self, Value};

const HUB_URL: &str = "https://koji.fedoraproject.org/kojihub";
/// 200 lookups come to about 150 KB and take a few seconds. Fetching the whole
/// tag instead would download 28 MB, so we only ever ask about what's installed.
const BATCH_SIZE: usize = 200;
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

/// Looks up packages in a release tag such as `f45`. Packages Koji has never
/// heard of are simply absent from the returned map.
pub fn list_packages(
    transport: &dyn Transport,
    tag: &str,
    names: &[String],
) -> Result<HashMap<String, KojiPackage>> {
    let mut found = HashMap::new();
    // One request at a time, on purpose. Koji is shared infrastructure.
    for batch in names.chunks(BATCH_SIZE) {
        let request = multicall_request(tag, batch);
        let body = transport
            .post_xml(HUB_URL, request)?
            .into_success_body(HUB_URL)?;
        read_batch(&xmlrpc::parse_response(&body)?, batch, &mut found)?;
    }
    Ok(found)
}

fn read_batch(
    response: &Value,
    names: &[String],
    found: &mut HashMap<String, KojiPackage>,
) -> Result<()> {
    let results = response
        .as_array()
        .ok_or_else(|| parse_error("multicall result is not an array"))?;
    if results.len() != names.len() {
        return Err(parse_error(format!(
            "asked about {} packages but got {} answers",
            names.len(),
            results.len()
        )));
    }

    for (name, result) in names.iter().zip(results) {
        if let Some(package) = read_result(result)? {
            found.insert(name.clone(), package);
        }
    }
    Ok(())
}

/// Each answer is either a fault struct or `[[ {row}, ... ]]`.
fn read_result(result: &Value) -> Result<Option<KojiPackage>> {
    if result.get("faultString").is_some() {
        let message = xmlrpc::fault_message(result);
        return if message.starts_with("No such entry") {
            Ok(None)
        } else {
            Err(Error::KojiFault(message))
        };
    }

    let rows = result
        .as_array()
        .and_then(|outer| outer.first())
        .and_then(Value::as_array)
        .ok_or_else(|| parse_error("unexpected shape for a package result"))?;
    let Some(row) = rows.first() else {
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

fn parse_error(message: impl Into<String>) -> Error {
    Error::Parse {
        what: "Koji package data",
        message: message.into(),
    }
}

fn multicall_request(tag: &str, names: &[String]) -> String {
    let calls: String = names
        .iter()
        .map(|name| list_packages_call(tag, name))
        .collect();
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
        let request = multicall_request("f45", &["a&b".to_string()]);
        assert!(request.contains("a&amp;b"));
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
        assert!(matches!(read_result(&fault), Err(Error::KojiFault(_))));
    }
}
