use thiserror::Error;

/// Everything that can go wrong inside rpm-health.
///
/// Rust has no exceptions. A function that can fail returns `Result<T, E>`,
/// which is either `Ok(value)` or `Err(error)`, and the compiler makes the
/// caller deal with both. `#[derive(Error)]` writes the boilerplate that turns
/// each `#[error("...")]` message into the text shown to the user.
#[derive(Debug, Error)]
pub enum Error {
    #[error("could not read the installed package list: {0}")]
    Rpm(String),

    #[error("{0} isn't installed. Try the name of an RPM or its source package.")]
    NotInstalled(String),

    #[error("could not work out this system's Fedora release: {0}")]
    OsRelease(String),

    #[error("request to {url} failed: {message}")]
    Http { url: String, message: String },

    #[error("{url} returned HTTP {status}")]
    BadStatus { url: String, status: u16 },

    #[error("could not understand the {what}: {message}")]
    Parse { what: &'static str, message: String },

    #[error("Koji reported an error: {0}")]
    KojiFault(String),
}

/// Saves writing `Result<T, Error>` in every function signature.
pub type Result<T> = std::result::Result<T, Error>;
