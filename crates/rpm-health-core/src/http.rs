use std::cell::Cell;
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// Identifies us to Fedora's servers, so admins can see who is calling.
const USER_AGENT: &str = concat!(
    "rpm-health/",
    env!("CARGO_PKG_VERSION"),
    " (Fedora package health checker)"
);
const TIMEOUT: Duration = Duration::from_secs(120);
/// Without this, a connection that never answers can hang for over two minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// The pause between requests, to go easy on shared infrastructure.
const REQUEST_GAP: Duration = Duration::from_millis(500);
/// How long to wait before each retry. A scan can make over 100 requests, so one
/// network hiccup shouldn't throw away all the others.
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(5)];

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
    /// `Cell` allows changing this through `&self`, which is all the `Transport`
    /// trait gives us. It's fine here because we only make one request at a time.
    last_request_finished: Cell<Option<Instant>>,
}

impl HttpClient {
    pub fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            // Don't reuse connections. Fedora's servers can close an idle one at
            // the moment we try to reuse it, which fails the request with
            // "connection closed before message completed". Our requests are
            // few and sequential, so a new connection each time costs very little.
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| Error::Http {
                url: "(client setup)".to_string(),
                message: describe(&e),
            })?;
        Ok(HttpClient {
            client,
            last_request_finished: Cell::new(None),
        })
    }

    fn send(&self, mut attempt: impl FnMut() -> Result<Response>) -> Result<Response> {
        if let Some(finished) = self.last_request_finished.get() {
            thread::sleep(REQUEST_GAP.saturating_sub(finished.elapsed()));
        }
        let result = send_with_retries(&RETRY_DELAYS, &mut attempt, thread::sleep);
        self.last_request_finished.set(Some(Instant::now()));
        result
    }
}

/// Tries a request, then once per delay if it failed in a way that could pass.
/// Sleeping is a parameter so that tests don't have to wait.
fn send_with_retries(
    delays: &[Duration],
    attempt: &mut impl FnMut() -> Result<Response>,
    sleep: impl Fn(Duration),
) -> Result<Response> {
    let mut result = attempt();
    for delay in delays {
        if !is_worth_retrying(&result) {
            break;
        }
        sleep(*delay);
        result = attempt();
    }
    result
}

/// Network failures and overloaded servers can pass. A 404 won't.
fn is_worth_retrying(result: &Result<Response>) -> bool {
    match result {
        Ok(response) => matches!(response.status, 429 | 502 | 503 | 504),
        Err(Error::Http { .. }) => true,
        Err(_) => false,
    }
}

impl Transport for HttpClient {
    fn get(&self, url: &str) -> Result<Response> {
        self.send(|| read_response(url, self.client.get(url).send()))
    }

    fn post_xml(&self, url: &str, body: String) -> Result<Response> {
        // Each attempt needs its own copy of the body. Retrying a POST is safe
        // here, because the only ones we send are read-only Koji queries.
        self.send(|| {
            let request = self
                .client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "text/xml")
                .body(body.clone());
            read_response(url, request.send())
        })
    }
}

fn read_response(
    url: &str,
    result: reqwest::Result<reqwest::blocking::Response>,
) -> Result<Response> {
    let to_error = |e: reqwest::Error| Error::Http {
        url: url.to_string(),
        message: describe(&e),
    };
    let response = result.map_err(to_error)?;
    let status = response.status().as_u16();
    let body = response.text().map_err(to_error)?;
    Ok(Response { status, body })
}

/// Library errors often wrap the real cause, such as "connection reset", inside
/// a vaguer one. This follows the chain of `source()` errors to include it all.
///
/// `&dyn Error` accepts a reference to any type that implements the `Error`
/// trait, which lets the loop step through errors of different types.
fn describe(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
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
    use std::cell::RefCell;

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

    #[test]
    fn describe_includes_the_underlying_cause() {
        #[derive(Debug, thiserror::Error)]
        #[error("error sending request")]
        struct Outer(#[source] std::io::Error);

        let cause = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset");
        assert_eq!(
            describe(&Outer(cause)),
            "error sending request: connection reset"
        );
    }

    fn network_error() -> Result<Response> {
        Err(Error::Http {
            url: "http://x".to_string(),
            message: "connection timed out".to_string(),
        })
    }

    fn status(status: u16) -> Result<Response> {
        Ok(Response {
            status,
            body: String::new(),
        })
    }

    /// Plays back answers in order, and reports how many attempts were made.
    fn run(answers: Vec<Result<Response>>) -> (Result<Response>, usize) {
        let mut answers = answers.into_iter();
        let mut attempts = 0;
        let delays = [Duration::ZERO, Duration::ZERO];
        let result = send_with_retries(
            &delays,
            &mut || {
                attempts += 1;
                answers.next().unwrap()
            },
            |_| {},
        );
        (result, attempts)
    }

    #[test]
    fn success_is_not_retried() {
        let (result, attempts) = run(vec![status(200)]);
        assert_eq!(result.unwrap().status, 200);
        assert_eq!(attempts, 1);
    }

    #[test]
    fn a_network_error_is_retried_until_it_works() {
        let (result, attempts) = run(vec![network_error(), network_error(), status(200)]);
        assert_eq!(result.unwrap().status, 200);
        assert_eq!(attempts, 3);
    }

    #[test]
    fn it_gives_up_after_the_last_retry() {
        let (result, attempts) = run(vec![network_error(), network_error(), network_error()]);
        assert!(result.is_err());
        assert_eq!(attempts, 3);
    }

    #[test]
    fn overloaded_servers_are_retried() {
        let (result, attempts) = run(vec![status(503), status(429), status(200)]);
        assert_eq!(result.unwrap().status, 200);
        assert_eq!(attempts, 3);
    }

    #[test]
    fn a_404_is_final() {
        let (result, attempts) = run(vec![status(404)]);
        assert_eq!(result.unwrap().status, 404);
        assert_eq!(attempts, 1);
    }

    #[test]
    fn it_waits_the_given_delay_before_each_retry() {
        let waited = RefCell::new(Vec::new());
        let mut answers = vec![network_error(), status(200)].into_iter();
        let delays = [Duration::from_secs(2), Duration::from_secs(5)];

        send_with_retries(&delays, &mut || answers.next().unwrap(), |delay| {
            waited.borrow_mut().push(delay)
        })
        .unwrap();

        assert_eq!(*waited.borrow(), [Duration::from_secs(2)]);
    }
}
