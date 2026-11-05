//! Saves responses on disk, so repeat scans don't hammer Fedora's servers.
//!
//! This wraps another `Transport`, so the data sources and signals don't know
//! that caching exists. Each response is its own file, which also means a scan
//! that fails on page 67 of 104 resumes from there next time.
//!
//! The cache is only ever a shortcut. If it can't be read or written, the scan
//! still works, just more slowly. Nothing is written outside the user's own cache
//! directory.

use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::http::{Response, Transport};
use crate::sources::{distgit, koji};

/// Fedora's data doesn't change quickly, and neither do packages' fortunes.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Files this old are deleted, because changing the installed packages can leave
/// behind entries that no request will ever ask for again.
const DELETE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const FILE_EXTENSION: &str = "json";

/// Finds where the cache belongs, following the XDG standard: `$XDG_CACHE_HOME`,
/// or else `~/.cache`. Returns `None` if neither is available.
pub fn default_dir() -> Option<PathBuf> {
    dir_from(
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn dir_from(xdg_cache_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    // The standard says a relative $XDG_CACHE_HOME must be ignored.
    let base = xdg_cache_home
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| home.join(".cache")))?;
    Some(base.join("rpm-health"))
}

/// What one saved response looks like on disk.
#[derive(Serialize, Deserialize)]
struct Entry {
    url: String,
    fetched_at: u64,
    status: u16,
    body: String,
}

/// A directory of saved responses.
struct Store {
    dir: PathBuf,
}

impl Store {
    /// A POST to one URL can ask many different things, so the body is part of
    /// the key. `DefaultHasher` isn't stable across Rust versions, but the worst
    /// that can do is cause some cache misses.
    fn path_for(&self, url: &str, body: Option<&str>) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        url.hash(&mut hasher);
        body.hash(&mut hasher);
        let name = format!("{:016x}.{FILE_EXTENSION}", hasher.finish());
        self.dir.join(name)
    }

    /// Returns the response and its age in seconds. Anything unreadable, expired
    /// or from the future counts as not being there.
    fn load(&self, url: &str, body: Option<&str>, now: u64) -> Option<(Response, u64)> {
        let text = fs::read_to_string(self.path_for(url, body)).ok()?;
        let entry: Entry = serde_json::from_str(&text).ok()?;
        let age = now.checked_sub(entry.fetched_at)?;
        if entry.url != url || age >= MAX_AGE.as_secs() {
            return None;
        }
        Some((Response::new(entry.status, entry.body), age))
    }

    /// Writes to a temporary file first, then renames it, because a rename either
    /// happens completely or not at all. An interrupted scan can't leave half a
    /// file behind.
    fn save(&self, url: &str, body: Option<&str>, response: &Response, now: u64) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let entry = Entry {
            url: url.to_string(),
            fetched_at: now,
            status: response.status,
            body: response.body.clone(),
        };
        let path = self.path_for(url, body);
        let temporary = path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&temporary, serde_json::to_vec(&entry)?)?;
        fs::rename(&temporary, &path).inspect_err(|_| {
            // We're already reporting the rename failure, so ignore this one.
            let _ = fs::remove_file(&temporary);
        })
    }

    /// Best effort, because leftover files do no harm.
    fn delete_old_files(&self) {
        let Ok(files) = fs::read_dir(&self.dir) else {
            return;
        };
        for file in files.flatten() {
            let path = file.path();
            if is_too_old(&path) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn is_too_old(path: &Path) -> bool {
    let is_cache_file = path.extension().is_some_and(|ext| ext == FILE_EXTENSION);
    let age = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
    is_cache_file && age.is_some_and(|age| age > DELETE_AFTER)
}

/// How a scan used the cache, so the user can tell how fresh their results are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub reused: usize,
    pub fetched: usize,
    pub oldest_reused_seconds: Option<u64>,
    pub failed_writes: usize,
}

/// A `Transport` that answers from disk when it can, and otherwise asks `T`.
///
/// `<T: Transport>` makes this generic: it works with any `Transport`, whether
/// that's the real client or a fake in tests, and the compiler checks that.
pub struct CachingTransport<T: Transport> {
    inner: T,
    store: Option<Store>,
    refresh: bool,
    clock: fn() -> u64,
    stats: Cell<CacheStats>,
}

impl<T: Transport> CachingTransport<T> {
    /// With no `dir`, nothing is cached. With `refresh`, saved responses are
    /// ignored, but new ones are still saved for next time.
    pub fn new(inner: T, dir: Option<PathBuf>, refresh: bool) -> Self {
        Self::with_clock(inner, dir, refresh, now_seconds)
    }

    fn with_clock(inner: T, dir: Option<PathBuf>, refresh: bool, clock: fn() -> u64) -> Self {
        let store = dir.map(|dir| Store { dir });
        if let Some(store) = &store {
            store.delete_old_files();
        }
        CachingTransport {
            inner,
            store,
            refresh,
            clock,
            stats: Cell::new(CacheStats::default()),
        }
    }

    pub fn stats(&self) -> CacheStats {
        self.stats.get()
    }

    fn fetch(
        &self,
        url: &str,
        body: Option<&str>,
        request: impl FnOnce() -> Result<Response>,
    ) -> Result<Response> {
        let now = (self.clock)();
        let mut stats = self.stats.get();

        let saved = match &self.store {
            Some(store) if !self.refresh => store.load(url, body, now),
            _ => None,
        };
        if let Some((response, age)) = saved {
            stats.reused += 1;
            stats.oldest_reused_seconds = Some(
                stats
                    .oldest_reused_seconds
                    .map_or(age, |oldest| oldest.max(age)),
            );
            self.stats.set(stats);
            return Ok(response);
        }

        let response = request()?;
        stats.fetched += 1;
        if let Some(store) = &self.store
            && is_worth_saving(url, body.is_some(), &response)
            && store.save(url, body, &response, now).is_err()
        {
            stats.failed_writes += 1;
        }
        self.stats.set(stats);
        Ok(response)
    }
}

impl<T: Transport> Transport for CachingTransport<T> {
    fn get(&self, url: &str) -> Result<Response> {
        self.fetch(url, None, || self.inner.get(url))
    }

    fn post_xml(&self, url: &str, body: String) -> Result<Response> {
        // Each path uses the body differently, so `fetch` borrows it, and only
        // a real request needs its own copy.
        self.fetch(url, Some(&body), || self.inner.post_xml(url, body.clone()))
    }
}

/// Saving a bad response would replay the failure for a whole day, so only
/// answers that look complete and correct are kept.
fn is_worth_saving(url: &str, is_post: bool, response: &Response) -> bool {
    match response.status {
        200 if is_post => koji::is_reusable_response(&response.body),
        200 => {
            let content_type = response.content_type.as_deref().unwrap_or_default();
            if content_type.contains("json") {
                serde_json::from_str::<serde_json::Value>(&response.body).is_ok()
            } else {
                !content_type.contains("html")
            }
        }
        // For a retirement note, "not found" is a real answer. Anywhere else, a
        // 404 may mean something is temporarily wrong.
        404 => distgit::is_dead_package_url(url),
        _ => false,
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::error::Error;
    use crate::http::fake::FakeTransport;

    const URL: &str = "https://example.com/data";
    const NOW: u64 = 1_000_000;
    const DAY: u64 = 24 * 60 * 60;

    /// A unique directory that deletes itself. `Drop` runs automatically when
    /// the value goes out of scope, even if the test fails.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let number = COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = format!("rpm-health-test-{}-{number}", std::process::id());
            TempDir(std::env::temp_dir().join(name))
        }

        fn path(&self) -> Option<PathBuf> {
            Some(self.0.clone())
        }

        fn file_count(&self) -> usize {
            fs::read_dir(&self.0).map_or(0, Iterator::count)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn at_now() -> u64 {
        NOW
    }

    fn almost_a_day_later() -> u64 {
        NOW + DAY - 1
    }

    fn a_day_later() -> u64 {
        NOW + DAY
    }

    fn before_now() -> u64 {
        NOW - 1
    }

    fn cached(
        dir: &TempDir,
        inner: FakeTransport,
        refresh: bool,
        clock: fn() -> u64,
    ) -> CachingTransport<FakeTransport> {
        CachingTransport::with_clock(inner, dir.path(), refresh, clock)
    }

    fn serving(body: &str) -> FakeTransport {
        FakeTransport::default().with_get(URL, body)
    }

    /// Saves one response, as if by an earlier scan.
    fn fill(dir: &TempDir, body: &str) {
        cached(dir, serving(body), false, at_now).get(URL).unwrap();
    }

    #[test]
    fn a_second_request_is_answered_from_disk() {
        let dir = TempDir::new();
        fill(&dir, "first");

        let later = cached(&dir, serving("second"), false, at_now);

        assert_eq!(later.get(URL).unwrap().body, "first");
        assert!(later.inner.get_urls.borrow().is_empty());
        assert_eq!(later.stats().reused, 1);
        assert_eq!(later.stats().fetched, 0);
    }

    #[test]
    fn responses_are_reused_for_almost_a_day() {
        let dir = TempDir::new();
        fill(&dir, "first");
        let later = cached(&dir, serving("second"), false, almost_a_day_later);
        assert_eq!(later.get(URL).unwrap().body, "first");
        assert_eq!(later.stats().oldest_reused_seconds, Some(DAY - 1));
    }

    #[test]
    fn expired_responses_are_fetched_again() {
        let dir = TempDir::new();
        fill(&dir, "first");
        let later = cached(&dir, serving("second"), false, a_day_later);
        assert_eq!(later.get(URL).unwrap().body, "second");
    }

    /// A clock that jumped backwards shouldn't make old data look fresh forever.
    #[test]
    fn responses_from_the_future_are_ignored() {
        let dir = TempDir::new();
        fill(&dir, "first");
        let later = cached(&dir, serving("second"), false, before_now);
        assert_eq!(later.get(URL).unwrap().body, "second");
    }

    #[test]
    fn refresh_ignores_the_cache_but_updates_it() {
        let dir = TempDir::new();
        fill(&dir, "first");

        let refreshed = cached(&dir, serving("second"), true, at_now);
        assert_eq!(refreshed.get(URL).unwrap().body, "second");
        assert_eq!(refreshed.stats().fetched, 1);

        let later = cached(&dir, serving("third"), false, at_now);
        assert_eq!(later.get(URL).unwrap().body, "second");
    }

    #[test]
    fn posts_with_different_bodies_are_kept_apart() {
        let dir = TempDir::new();
        let answer = |name: &str| {
            format!(
                "<methodResponse><params><param><value><array><data>\
                 <value><string>{name}</string></value></data></array></value>\
                 </param></params></methodResponse>"
            )
        };
        let transport = cached(
            &dir,
            FakeTransport::default()
                .with_post_reply(&answer("a"))
                .with_post_reply(&answer("b")),
            false,
            at_now,
        );

        let a = transport.post_xml(URL, "ask a".to_string()).unwrap().body;
        let b = transport.post_xml(URL, "ask b".to_string()).unwrap().body;
        let a_again = transport.post_xml(URL, "ask a".to_string()).unwrap().body;

        assert_ne!(a, b);
        assert_eq!(a, a_again);
        assert_eq!(transport.inner.post_bodies.borrow().len(), 2);
    }

    #[test]
    fn a_corrupt_file_counts_as_a_miss() {
        let dir = TempDir::new();
        fill(&dir, "first");
        let store = Store { dir: dir.0.clone() };
        fs::write(store.path_for(URL, None), "{ not json").unwrap();

        let later = cached(&dir, serving("second"), false, at_now);

        assert_eq!(later.get(URL).unwrap().body, "second");
    }

    #[test]
    fn network_failures_pass_through_and_save_nothing() {
        let dir = TempDir::new();
        let transport = cached(
            &dir,
            FakeTransport::default().with_failing_get(URL),
            false,
            at_now,
        );

        assert!(matches!(transport.get(URL), Err(Error::Http { .. })));
        assert_eq!(dir.file_count(), 0);
    }

    /// The cache must never be able to break a scan.
    #[test]
    fn an_unwritable_cache_still_returns_the_response() {
        let dir = TempDir::new();
        fs::write(&dir.0, "a file where the directory should be").unwrap();
        let transport = CachingTransport::with_clock(serving("data"), dir.path(), false, at_now);

        assert_eq!(transport.get(URL).unwrap().body, "data");
        assert_eq!(transport.stats().failed_writes, 1);
        fs::remove_file(&dir.0).unwrap();
    }

    #[test]
    fn with_no_directory_everything_is_fetched() {
        let transport = CachingTransport::with_clock(serving("data"), None, false, at_now);
        transport.get(URL).unwrap();
        transport.get(URL).unwrap();
        assert_eq!(transport.stats().fetched, 2);
    }

    fn with_type(status: u16, content_type: &str, body: &str) -> Response {
        Response {
            content_type: Some(content_type.to_string()),
            ..Response::new(status, body.to_string())
        }
    }

    #[test]
    fn only_good_responses_are_saved() {
        let json = "application/json";
        assert!(is_worth_saving(URL, false, &with_type(200, json, "{}")));
        assert!(!is_worth_saving(
            URL,
            false,
            &with_type(200, json, "{ cut off")
        ));
        assert!(!is_worth_saving(
            URL,
            false,
            &with_type(200, "text/html", "<h1>Down</h1>")
        ));
        assert!(is_worth_saving(
            URL,
            false,
            &with_type(200, "text/plain", "Dead")
        ));
        assert!(!is_worth_saving(URL, false, &with_type(503, json, "{}")));
        assert!(!is_worth_saving(
            URL,
            true,
            &with_type(200, "text/xml", "not xml-rpc")
        ));
    }

    #[test]
    fn a_404_is_only_saved_for_retirement_notes() {
        let note = distgit::dead_package_url("htop", "f45");
        assert!(is_worth_saving(
            &note,
            false,
            &Response::new(404, String::new())
        ));
        assert!(!is_worth_saving(
            URL,
            false,
            &Response::new(404, String::new())
        ));
    }

    #[test]
    fn old_files_are_deleted_but_only_cache_files() {
        let dir = TempDir::new();
        fs::create_dir_all(&dir.0).unwrap();
        let old_cache = dir.0.join("old.json");
        let old_other = dir.0.join("notes.txt");
        for path in [&old_cache, &old_other] {
            let file = fs::File::create(path).unwrap();
            let long_ago = SystemTime::now() - DELETE_AFTER - Duration::from_secs(60);
            file.set_modified(long_ago).unwrap();
        }
        let recent = dir.0.join("recent.json");
        fs::write(&recent, "{}").unwrap();

        Store { dir: dir.0.clone() }.delete_old_files();

        assert!(!old_cache.exists());
        assert!(old_other.exists());
        assert!(recent.exists());
    }

    #[test]
    fn the_cache_directory_follows_the_xdg_standard() {
        let xdg = Some(PathBuf::from("/xdg"));
        let home = Some(PathBuf::from("/home/me"));
        let relative = Some(PathBuf::from("relative"));

        assert_eq!(
            dir_from(xdg, home.clone()),
            Some(PathBuf::from("/xdg/rpm-health"))
        );
        assert_eq!(
            dir_from(None, home.clone()),
            Some(PathBuf::from("/home/me/.cache/rpm-health"))
        );
        assert_eq!(
            dir_from(relative, home),
            Some(PathBuf::from("/home/me/.cache/rpm-health"))
        );
        assert_eq!(dir_from(None, None), None);
    }
}
