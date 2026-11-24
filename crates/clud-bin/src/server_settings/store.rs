//! Cache, refresh timing, and fetch for server settings.
//!
//! A cache younger than [`REFRESH_INTERVAL`] is used with no request. Otherwise
//! a named background thread fetches, merges, and atomically writes the cache,
//! and the caller waits at most `wait` for it: on a healthy network an edit
//! applies to the same launch, offline the cost is bounded, and a slow refresh
//! still lands for the next launch. A failed attempt is not retried until the
//! interval passes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use super::json::MAX_DOCUMENT_BYTES;
use super::sections::SECTIONS;
use super::{merge, trace, Document, Origin, Snapshot};

/// Freshness of the cache, and the backoff after a failed attempt.
/// `raw.githubusercontent.com` caches for five minutes, so polling faster
/// would not see edits sooner.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Longest a launch waits for a refresh before continuing on the cache.
pub(crate) const REFRESH_WAIT: Duration = Duration::from_millis(750);
/// The refresh thread's own limit; it may outlive the launch's wait.
pub(super) const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Clock skew tolerated before a future file mtime stops counting as "now".
const MTIME_SKEW: Duration = Duration::from_secs(60);

pub(crate) type Fetch = Arc<dyn Fn() -> Result<Vec<u8>, String> + Send + Sync>;

/// Everything [`load`] reads from outside, so timing and fallback are testable
/// without a network or a real home directory.
pub(crate) struct Sources {
    /// `None` skips the cache and backoff and always fetches (a draft URL).
    pub cache_path: Option<PathBuf>,
    pub now: SystemTime,
    pub fetch: Fetch,
    pub wait: Duration,
}

pub(crate) struct Loaded {
    pub snapshot: Snapshot,
    /// The refresh thread, when one ran. A launch detaches it; tests join it.
    pub refresh: Option<JoinHandle<()>>,
}

pub(crate) fn load(sources: Sources) -> Loaded {
    let cached = sources.cache_path.as_deref().and_then(read_cache);
    let on_cache = |refresh| Loaded {
        snapshot: cached
            .clone()
            .map_or_else(Snapshot::built_in_only, |document| {
                Snapshot::from_document(document, Origin::Cached)
            }),
        refresh,
    };

    if let Some(path) = sources.cache_path.as_deref() {
        if cached.is_some() && is_younger_than(path, sources.now, REFRESH_INTERVAL) {
            return on_cache(None);
        }
        let stamp = attempt_stamp(path);
        if is_younger_than(&stamp, sources.now, REFRESH_INTERVAL) {
            return on_cache(None);
        }
        record_attempt(&stamp);
    }

    let (sender, receiver) = mpsc::channel();
    let previous = cached.clone();
    let cache_path = sources.cache_path.clone();
    let fetch = Arc::clone(&sources.fetch);
    let spawned = std::thread::Builder::new()
        .name("clud-server-settings-refresh".to_string())
        .spawn(move || {
            let refreshed = refresh(fetch.as_ref(), previous.as_ref(), cache_path.as_deref());
            let _ = sender.send(refreshed);
        });
    let handle = match spawned {
        Ok(handle) => handle,
        Err(error) => {
            trace(format_args!("could not start a refresh: {error}"));
            return on_cache(None);
        }
    };
    match receiver.recv_timeout(sources.wait) {
        Ok(Some(document)) => Loaded {
            snapshot: Snapshot::from_document(document, Origin::Served),
            refresh: Some(handle),
        },
        Ok(None) => on_cache(Some(handle)),
        Err(_) => {
            trace(format_args!(
                "refresh still running after {:?}; continuing on the cached copy",
                sources.wait
            ));
            on_cache(Some(handle))
        }
    }
}

/// Fetch, validate, merge over the previous cache, and write the result.
/// `None` when nothing usable was served, which leaves the cache untouched.
fn refresh(
    fetch: &(dyn Fn() -> Result<Vec<u8>, String> + Send + Sync),
    previous: Option<&Document>,
    cache_path: Option<&Path>,
) -> Option<Document> {
    let body = fetch()
        .map_err(|error| trace(format_args!("fetch failed: {error}")))
        .ok()?;
    let served = Document::parse(&body)
        .map_err(|error| {
            trace(format_args!(
                "served document rejected: {error}; keeping the last good copy"
            ))
        })
        .ok()?;
    let (merged, rejected) = merge(&served, previous, SECTIONS);
    for (key, reason) in &rejected {
        trace(format_args!(
            "section `{key}` rejected: {reason}; keeping its last good value"
        ));
    }
    if let Some(path) = cache_path {
        write_cache(path, &merged);
    }
    Some(merged)
}

pub(crate) fn fetch_url(url: &str) -> Result<Vec<u8>, String> {
    let response = ureq::AgentBuilder::new()
        .timeout(FETCH_TIMEOUT)
        .build()
        .get(url)
        .call()
        .map_err(|error| error.to_string())?;
    let mut body = Vec::new();
    // One byte past the limit is enough for the parser to reject an oversized
    // body without buffering all of it.
    response
        .into_reader()
        .take(MAX_DOCUMENT_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|error| error.to_string())?;
    Ok(body)
}

/// A cache is re-validated on every read and never trusted when corrupt.
fn read_cache(path: &Path) -> Option<Document> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_DOCUMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    Document::parse(&bytes)
        .map_err(|error| {
            trace(format_args!(
                "ignoring corrupt cache {}: {error}",
                path.display()
            ))
        })
        .ok()
}

fn attempt_stamp(cache_path: &Path) -> PathBuf {
    cache_path.with_extension("json.last-attempt")
}

fn is_younger_than(path: &Path, now: SystemTime, limit: Duration) -> bool {
    let Ok(modified) = std::fs::metadata(path).and_then(|metadata| metadata.modified()) else {
        return false;
    };
    match now.duration_since(modified) {
        Ok(age) => age < limit,
        Err(error) => error.duration() < MTIME_SKEW,
    }
}

fn record_attempt(stamp: &Path) {
    if let Some(parent) = stamp.parent() {
        if std::fs::create_dir_all(parent).is_ok() {
            let _ = std::fs::write(stamp, b"");
        }
    }
}

/// Stage then rename, so a concurrent reader or an exit mid-write never sees
/// a torn file.
fn write_cache(path: &Path, document: &Document) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let staging = path.with_extension("json.partial");
    if std::fs::write(&staging, document.to_json())
        .and_then(|()| std::fs::rename(&staging, path))
        .is_err()
    {
        let _ = std::fs::remove_file(&staging);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_settings::{built_in, DeepSeekSettings};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const LONG_WAIT: Duration = Duration::from_secs(30);

    fn served(default_model: &str) -> Vec<u8> {
        format!(
            r#"{{"schema_version":1,"sections":{{"deepseek":{{"default_model":"{default_model}","subagent_model":"deepseek-flash"}},"future":{{"x":1}}}}}}"#
        )
        .into_bytes()
    }

    fn counting(
        body: impl Fn() -> Result<Vec<u8>, String> + Send + Sync + 'static,
    ) -> (Fetch, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let fetch: Fetch = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            body()
        });
        (fetch, calls)
    }

    fn load_joined(cache_path: Option<PathBuf>, now: SystemTime, fetch: Fetch) -> Snapshot {
        let loaded = load(Sources {
            cache_path,
            now,
            fetch,
            wait: LONG_WAIT,
        });
        if let Some(handle) = loaded.refresh {
            handle.join().unwrap();
        }
        loaded.snapshot
    }

    fn cache_in(dir: &tempfile::TempDir) -> PathBuf {
        dir.path()
            .join("server-settings")
            .join("server-settings.json")
    }

    fn seed_cache(path: &Path, default_model: &str) {
        write_cache(path, &Document::parse(&served(default_model)).unwrap());
    }

    fn later() -> SystemTime {
        SystemTime::now() + REFRESH_INTERVAL + Duration::from_secs(60)
    }

    fn default_model(snapshot: &Snapshot) -> String {
        snapshot.section::<DeepSeekSettings>().default_model
    }

    #[test]
    fn a_missing_cache_is_filled_from_the_served_document() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(&dir);
        let (fetch, calls) = counting(|| Ok(served("deepseek-flash-2[1m]")));
        let snapshot = load_joined(Some(cache.clone()), SystemTime::now(), fetch);
        assert_eq!(snapshot.origin(), Origin::Served);
        assert_eq!(default_model(&snapshot), "deepseek-flash-2[1m]");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let cached = read_cache(&cache).unwrap();
        assert!(
            cached.sections.contains_key("future"),
            "unknown sections are kept"
        );
    }

    #[test]
    fn a_fresh_cache_is_used_without_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(&dir);
        seed_cache(&cache, "deepseek-flash-2");
        let (fetch, calls) = counting(|| Ok(served("deepseek-flash-3")));
        let snapshot = load_joined(Some(cache), SystemTime::now(), fetch);
        assert_eq!(snapshot.origin(), Origin::Cached);
        assert_eq!(default_model(&snapshot), "deepseek-flash-2");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_stale_cache_is_refreshed_for_the_same_launch() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(&dir);
        seed_cache(&cache, "deepseek-flash-2");
        let (fetch, _) = counting(|| Ok(served("deepseek-flash-3")));
        let snapshot = load_joined(Some(cache.clone()), later(), fetch);
        assert_eq!(snapshot.origin(), Origin::Served);
        assert_eq!(default_model(&snapshot), "deepseek-flash-3");
    }

    #[test]
    fn a_slow_refresh_leaves_the_launch_on_its_cache_and_lands_for_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(&dir);
        seed_cache(&cache, "deepseek-flash-2");
        let (fetch, _) = counting(|| {
            std::thread::sleep(Duration::from_millis(300));
            Ok(served("deepseek-flash-3"))
        });
        let loaded = load(Sources {
            cache_path: Some(cache.clone()),
            now: later(),
            fetch,
            wait: Duration::from_millis(10),
        });
        assert_eq!(loaded.snapshot.origin(), Origin::Cached);
        assert_eq!(default_model(&loaded.snapshot), "deepseek-flash-2");
        loaded.refresh.unwrap().join().unwrap();
        assert_eq!(
            read_cache(&cache).unwrap().sections["deepseek"]["default_model"],
            "deepseek-flash-3"
        );
    }

    #[test]
    fn a_malformed_document_keeps_the_last_good_cache_byte_for_byte() {
        for body in [
            b"<html>500</html>".to_vec(),
            b"{\"schema_version\":1,\"sections\":{}".to_vec(),
            b"{\"schema_version\":9,\"sections\":{}}".to_vec(),
            b"{\"schema_version\":1,\"sections\":{},\"sections\":{}}".to_vec(),
            Vec::new(),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let cache = cache_in(&dir);
            seed_cache(&cache, "deepseek-flash-2");
            let before = std::fs::read(&cache).unwrap();
            let (fetch, _) = counting(move || Ok(body.clone()));
            let snapshot = load_joined(Some(cache.clone()), later(), fetch);
            assert_eq!(snapshot.origin(), Origin::Cached);
            assert_eq!(default_model(&snapshot), "deepseek-flash-2");
            assert_eq!(std::fs::read(&cache).unwrap(), before);
        }
    }

    #[test]
    fn an_invalid_served_section_keeps_its_last_good_value() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(&dir);
        seed_cache(&cache, "deepseek-flash-2");
        let (fetch, _) = counting(|| Ok(served("gpt-5.6-terra")));
        let snapshot = load_joined(Some(cache.clone()), later(), fetch);
        assert_eq!(snapshot.origin(), Origin::Served);
        assert_eq!(default_model(&snapshot), "deepseek-flash-2");
        assert_eq!(
            read_cache(&cache).unwrap().sections["deepseek"]["default_model"],
            "deepseek-flash-2"
        );
    }

    #[test]
    fn a_section_removed_on_the_server_resets_to_built_in() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(&dir);
        seed_cache(&cache, "deepseek-flash-2");
        let (fetch, _) = counting(|| Ok(br#"{"schema_version":1,"sections":{}}"#.to_vec()));
        let snapshot = load_joined(Some(cache), later(), fetch);
        assert_eq!(
            snapshot.section::<DeepSeekSettings>(),
            built_in::<DeepSeekSettings>()
        );
    }

    #[test]
    fn a_failed_attempt_is_not_retried_until_the_interval_passes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(&dir);
        let (fetch, calls) = counting(|| Err("unreachable".to_string()));
        let now = SystemTime::now();

        let snapshot = load_joined(Some(cache.clone()), now, Arc::clone(&fetch));
        assert_eq!(snapshot.origin(), Origin::BuiltIn);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        load_joined(Some(cache.clone()), now, Arc::clone(&fetch));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a failure must back off");

        load_joined(Some(cache), later(), fetch);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_corrupt_cache_is_ignored_and_replaced_by_a_good_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(&dir);
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, b"{\"schema_version\":1,\"sect").unwrap();
        let (fetch, _) = counting(|| Err("offline".to_string()));
        let snapshot = load_joined(Some(cache.clone()), SystemTime::now(), fetch);
        assert_eq!(snapshot.origin(), Origin::BuiltIn);

        let (fetch, _) = counting(|| Ok(served("deepseek-flash-2")));
        let snapshot = load_joined(Some(cache.clone()), later(), fetch);
        assert_eq!(default_model(&snapshot), "deepseek-flash-2");
        assert!(read_cache(&cache).is_some());
    }

    #[test]
    fn without_a_cache_location_every_load_fetches() {
        let (fetch, calls) = counting(|| Ok(served("deepseek-flash-2")));
        let snapshot = load_joined(None, SystemTime::now(), Arc::clone(&fetch));
        assert_eq!(default_model(&snapshot), "deepseek-flash-2");
        load_joined(None, SystemTime::now(), fetch);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let (fetch, _) = counting(|| Err("offline".to_string()));
        assert_eq!(
            load_joined(None, SystemTime::now(), fetch).origin(),
            Origin::BuiltIn
        );
    }
}
