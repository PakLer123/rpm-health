use std::time::Duration;

use crate::error::{Error, Result};

/// Identifies us to Fedora's servers, so admins can see who is calling.
const USER_AGENT: &str = concat!(
    "rpm-health/",
    env!("CARGO_PKG_VERSION"),
    " (Fedora package health checker)"
);
const TIMEOUT: Duration = Duration::from_secs(120);

/// What the rest of the code needs from an HTTP response.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    /// Returns the body, or an error if the server didn't answer with a 2xx.
    ///
    /// Taking `self` by value moves the response into this method, so the
    /// caller no longer owns it. That lets us hand back the `body` String
    /// without copying it.
    pub fn into_success_body(self, url: &str) -> Result<String> {
        if (200..300).contains(&self.status) {
            Ok(self.body)
        } else {
            Err(Error::BadStatus {
                url: url.to_string(),
                status: self.status,
            })
        }
    }
}

/// Anything that can perform our HTTP requests.
///
/// A trait is a set of methods a type promises to provide, a bit like an
/// interface. Data sources only ever talk to a `Transport`, which lets tests
/// swap in a fake and run with no network at all.
pub trait Transport {
    fn get(&self, url: &str) -> Result<Response>;
    fn post_xml(&self, url: &str, body: String) -> Result<Response>;
}

/// The real `Transport`, backed by reqwest.
pub struct HttpClient {
    client: reqwest::blocking::Client,
}

impl HttpClient {
    pub fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| Error::Http {
                url: "(client setup)".to_string(),
                message: e.to_string(),
            })?;
        Ok(HttpClient { client })
    }
}

impl Transport for HttpClient {
    fn get(&self, url: &str) -> Result<Response> {
        read_response(url, self.client.get(url).send())
    }

    fn post_xml(&self, url: &str, body: String) -> Result<Response> {
        let request = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "text/xml")
            .body(body);
        read_response(url, request.send())
    }
}

fn read_response(
    url: &str,
    result: reqwest::Result<reqwest::blocking::Response>,
) -> Result<Response> {
    let to_error = |e: reqwest::Error| Error::Http {
        url: url.to_string(),
        message: e.to_string(),
    };
    let response = result.map_err(to_error)?;
    let status = response.status().as_u16();
    let body = response.text().map_err(to_error)?;
    Ok(Response { status, body })
}

/// A `Transport` for tests. `#[cfg(test)]` means this module only exists when
/// running `cargo test`, so it never ends up in the real program.
#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet, VecDeque};

    use super::*;

    /// GETs answer from a table, with 404 for unknown URLs. POSTs answer from
    /// a queue, in the order they arrive.
    #[derive(Default)]
    pub struct FakeTransport {
        pub gets: HashMap<String, String>,
        pub failing_gets: HashSet<String>,
        pub post_replies: RefCell<VecDeque<String>>,
        pub post_bodies: RefCell<Vec<String>>,
    }

    impl FakeTransport {
        pub fn with_get(mut self, url: &str, body: &str) -> Self {
            self.gets.insert(url.to_string(), body.to_string());
            self
        }

        pub fn with_failing_get(mut self, url: &str) -> Self {
            self.failing_gets.insert(url.to_string());
            self
        }

        pub fn with_post_reply(self, body: &str) -> Self {
            self.post_replies.borrow_mut().push_back(body.to_string());
            self
        }
    }

    impl Transport for FakeTransport {
        fn get(&self, url: &str) -> Result<Response> {
            if self.failing_gets.contains(url) {
                return Err(Error::Http {
                    url: url.to_string(),
                    message: "simulated network failure".to_string(),
                });
            }
            Ok(match self.gets.get(url) {
                Some(body) => Response {
                    status: 200,
                    body: body.clone(),
                },
                None => Response {
                    status: 404,
                    body: String::new(),
                },
            })
        }

        // `get` and `post_xml` only borrow `&self` immutably, as the trait
        // says. `RefCell` lets the fake still record what it was sent.
        fn post_xml(&self, url: &str, body: String) -> Result<Response> {
            self.post_bodies.borrow_mut().push(body);
            let reply = self.post_replies.borrow_mut().pop_front();
            let body = reply.unwrap_or_else(|| panic!("unexpected POST to {url}"));
            Ok(Response { status: 200, body })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_status_returns_the_body() {
        let response = Response {
            status: 200,
            body: "hello".to_string(),
        };
        assert_eq!(response.into_success_body("http://x").unwrap(), "hello");
    }

    #[test]
    fn error_status_becomes_an_error() {
        let response = Response {
            status: 503,
            body: String::new(),
        };
        assert!(matches!(
            response.into_success_body("http://x"),
            Err(Error::BadStatus { status: 503, .. })
        ));
    }
}
